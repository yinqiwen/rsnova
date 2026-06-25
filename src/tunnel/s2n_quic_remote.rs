use anyhow::Result;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::{net::SocketAddr, path::Path};

use crate::mux::event;
use crate::tunnel::stream::handle_server_stream;
use crate::tunnel::tunnel_registry::SharedRegistry;

/// Maximum concurrent outbound TCP connections per QUIC client connection.
const MAX_QUIC_PROXY_STREAMS: usize = 256;

static QUIC_CONN_ID: AtomicU32 = AtomicU32::new(0);

pub async fn start_quic_remote_server(
    listen: &SocketAddr,
    cert_path: &Path,
    key_path: &Path,
    idle_timeout_secs: usize,
    registry: Option<SharedRegistry>,
) -> Result<()> {
    let io = s2n_quic::provider::io::tokio::Builder::default()
        .with_receive_address(*listen)?
        .build()?;
    let mut server = s2n_quic::Server::builder()
        .with_tls((cert_path, key_path))?
        .with_io(io)?
        .start()?;

    while let Some(mut connection) = server.accept().await {
        let registry = registry.clone();
        tracing::info!("QUIC connection incoming");
        tokio::spawn(async move {
            let auth_stream = match connection.accept_bidirectional_stream().await {
                Ok(Some(stream)) => stream,
                Ok(None) => {
                    tracing::debug!("QUIC connection closed before auth stream");
                    return;
                }
                Err(e) => {
                    tracing::debug!("Failed to accept QUIC auth stream: {}", e);
                    return;
                }
            };
            let (mut recv, mut send) = auth_stream.split();

            let ev = match event::read_event(&mut recv).await {
                Ok(ev) => ev,
                Err(e) => {
                    tracing::debug!("Failed to read QUIC auth event: {}", e);
                    return;
                }
            };
            if ev.header.flags() != event::FLAG_AUTH {
                tracing::debug!(
                    "Expected FLAG_AUTH on QUIC auth stream, got flag={}",
                    ev.header.flags()
                );
                return;
            }

            let config = bincode::config::standard();
            let auth_req: event::AuthRequest =
                match bincode::decode_from_slice(ev.body.as_ref(), config) {
                    Ok((req, _)) => req,
                    Err(e) => {
                        tracing::debug!("Failed to decode QUIC AuthRequest: {}", e);
                        return;
                    }
                };

            match auth_req {
                event::AuthRequest::Proxy => {
                    let ack = event::AuthAck::Proxy;
                    let _ =
                        event::write_event(&mut send, event::new_auth_ack_event(0, &ack).unwrap())
                            .await;
                    drop(recv);
                    drop(send);

                    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_QUIC_PROXY_STREAMS));
                    let emfile_count = Arc::new(AtomicU32::new(0));

                    while let Ok(Some(stream)) = connection.accept_bidirectional_stream().await {
                        // Backpressure: if recent streams hit EMFILE, pause
                        let recent_emfile = emfile_count.swap(0, Ordering::Relaxed);
                        if recent_emfile > 0 {
                            let backoff = tokio::time::Duration::from_millis(
                                100 * recent_emfile.min(10) as u64,
                            );
                            tracing::warn!(
                                "QUIC EMFILE backpressure ({} recent), sleeping {:?}",
                                recent_emfile,
                                backoff,
                            );
                            tokio::time::sleep(backoff).await;
                        }

                        let permit = match semaphore.clone().try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                tracing::warn!(
                                    "QUIC max concurrent proxy streams ({}) reached, dropping stream",
                                    MAX_QUIC_PROXY_STREAMS,
                                );
                                metrics::counter!("quic_server_proxy_streams_rejected")
                                    .increment(1);
                                continue;
                            }
                        };

                        metrics::gauge!("quic_server_proxy_streams").increment(1.0);
                        let (mut r, mut s) = stream.split();
                        let emfile_ref = emfile_count.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(e) =
                                handle_server_stream(&mut r, &mut s, idle_timeout_secs).await
                            {
                                let err_str = e.to_string();
                                if err_str.contains("file descriptor")
                                    || err_str.contains("os error 24")
                                {
                                    emfile_ref.fetch_add(1, Ordering::Relaxed);
                                    metrics::counter!("quic_server_emfile").increment(1);
                                }
                                tracing::error!("failed: {reason}", reason = err_str);
                            }
                            metrics::gauge!("quic_server_proxy_streams").decrement(1.0);
                        });
                    }
                }
                event::AuthRequest::Register(register_req) => {
                    let results = if let Some(registry) = registry {
                        let conn_id = QUIC_CONN_ID.fetch_add(1, Ordering::Relaxed);
                        let (handle, mut acceptor) = connection.split();

                        let results = crate::tunnel::tunnel_remote::handle_tunnel_register(
                            &registry,
                            &register_req,
                            crate::tunnel::tunnel_registry::ConnectionHandler::Quic(handle.clone()),
                            conn_id,
                            idle_timeout_secs,
                        )
                        .await;

                        let client_id = register_req.client_id.clone();
                        let registry_clone = registry.clone();
                        tokio::spawn(async move {
                            while acceptor
                                .accept_bidirectional_stream()
                                .await
                                .is_ok_and(|v| v.is_some())
                            {}
                            tracing::info!("QUIC tunnel client '{}' disconnected", client_id);
                            let mut reg = registry_clone.lock().await;
                            let no_connections = reg.remove_connection(&client_id, conn_id);
                            if no_connections {
                                let empty_ports = reg.remove_client_routes(&client_id);
                                for port in empty_ports {
                                    if let Some(port_state) = reg.ports.remove(&port) {
                                        port_state.cancel_token.cancel();
                                    }
                                }
                            }
                        });

                        results
                    } else {
                        register_req
                            .tunnels
                            .iter()
                            .map(|t| event::TunnelResult {
                                success: false,
                                remote_port: t.remote_port,
                                sni: t.sni.clone(),
                                error: Some("tunnel not enabled on server".to_string()),
                            })
                            .collect()
                    };

                    let ack = event::AuthAck::RegisterAck(event::RegisterAck { results });
                    let _ =
                        event::write_event(&mut send, event::new_auth_ack_event(0, &ack).unwrap())
                            .await;
                    drop(recv);
                    drop(send);
                }
            }
        });
    }
    Ok(())
}
