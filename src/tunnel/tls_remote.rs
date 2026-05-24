// use anyhow::Context;
use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::mux::event;
use crate::tunnel::stream::handle_server_stream;
use crate::{mux, tunnel::ALPN_QUIC_HTTP};

use std::{collections::VecDeque, net::SocketAddr, path::Path, sync::Mutex};

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::utils::read_private_key;
use crate::utils::read_tokio_tls_certs;

pub async fn start_tls_remote_server(
    listen: &SocketAddr,
    cert_path: &Path,
    key_path: &Path,
    idle_timeout_secs: usize,
    stream_window: u32,
    registry: Option<crate::tunnel::tunnel_registry::SharedRegistry>,
) -> Result<()> {
    let certs = read_tokio_tls_certs(cert_path)?;
    let key = read_private_key(key_path)?;

    let mut server_crypto = tokio_rustls::rustls::ServerConfig::builder()
        // .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    server_crypto.alpn_protocols = ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();

    let acceptor = TlsAcceptor::from(Arc::new(server_crypto));
    let listener = TcpListener::bind(listen).await?;
    tracing::info!("TLS server listening on {:?}", listen);

    let mut id: u32 = 0;
    let free_ids = Arc::new(Mutex::new(VecDeque::new()));
    loop {
        let (stream, _) = listener.accept().await?;
        let conn_id = if free_ids.lock().unwrap().is_empty() {
            id += 1;
            id - 1
        } else {
            free_ids.lock().unwrap().pop_front().unwrap()
        };
        let acceptor = acceptor.clone();
        let fut_free_ids = free_ids.clone();
        let registry = registry.clone();
        let fut = async move {
            let stream = acceptor.accept(stream).await?;
            tracing::info!("TLS connection incoming");
            handle_tls_connection(stream, conn_id, idle_timeout_secs, stream_window, registry)
                .await?;
            Ok(()) as Result<()>
        };

        tokio::spawn(async move {
            if let Err(e) = fut.await {
                tracing::error!("connection failed: {reason}", reason = e.to_string())
            }
            fut_free_ids.lock().unwrap().push_back(conn_id);
        });
    }
}

pub(crate) async fn handle_tls_connection<T: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    conn: T,
    id: u32,
    idle_timeout_secs: usize,
    stream_window: u32,
    registry: Option<crate::tunnel::tunnel_registry::SharedRegistry>,
) -> Result<()> {
    let (r, w) = tokio::io::split(conn);
    let mux_conn = Arc::new(mux::Connection::new_with_stream_window(
        r,
        w,
        mux::Mode::Server,
        id,
        stream_window,
    ));

    let auth_stream = mux_conn.accept_stream().await?;
    let (mut auth_r, mut auth_w) = tokio::io::split(auth_stream);

    let ev = event::read_event(&mut auth_r).await?;
    if ev.header.flags() != event::FLAG_AUTH {
        return Err(anyhow!(
            "expected FLAG_AUTH on first stream, got flag={}",
            ev.header.flags()
        ));
    }

    let config = bincode::config::standard();
    let (auth_req, _): (event::AuthRequest, usize) =
        bincode::decode_from_slice(ev.body.as_ref(), config)
            .map_err(|e| anyhow!("decode AuthRequest failed: {}", e))?;

    match auth_req {
        event::AuthRequest::Proxy => {
            let ack = event::AuthAck::Proxy;
            let ack_ev = event::new_auth_ack_event(0, &ack)?;
            event::write_event(&mut auth_w, ack_ev).await?;
            drop(auth_r);
            drop(auth_w);

            loop {
                let stream = mux_conn.accept_stream().await?;
                metrics::gauge!("tls_server_proxy_streams").increment(1.0);
                tokio::spawn(async move {
                    let stream_id = stream.id();
                    let (mut stream_reader, mut stream_writer) = tokio::io::split(stream);
                    if let Err(e) = handle_server_stream(
                        &mut stream_reader,
                        &mut stream_writer,
                        idle_timeout_secs,
                    )
                    .await
                    {
                        tracing::error!(
                            "[{}/{}]failed: {reason}",
                            id,
                            stream_id,
                            reason = e.to_string()
                        );
                    }
                    metrics::gauge!("tls_server_proxy_streams").decrement(1.0);
                });
            }
        }
        event::AuthRequest::Register(register_req) => {
            let Some(registry) = registry else {
                let ack = event::AuthAck::RegisterAck(event::RegisterAck {
                    results: register_req
                        .tunnels
                        .iter()
                        .map(|t| event::TunnelResult {
                            success: false,
                            remote_port: t.remote_port,
                            sni: t.sni.clone(),
                            error: Some("tunnel not enabled on server".to_string()),
                        })
                        .collect(),
                });
                let ack_ev = event::new_auth_ack_event(0, &ack)?;
                event::write_event(&mut auth_w, ack_ev).await?;
                return Ok(());
            };

            let results = crate::tunnel::tunnel_remote::handle_tunnel_register(
                &registry,
                &register_req,
                crate::tunnel::tunnel_registry::ConnectionHandler::Tls(mux_conn.clone()),
                id,
                idle_timeout_secs,
            )
            .await;

            let ack = event::AuthAck::RegisterAck(event::RegisterAck { results });
            let ack_ev = event::new_auth_ack_event(0, &ack)?;
            event::write_event(&mut auth_w, ack_ev).await?;
            drop(auth_r);
            drop(auth_w);

            tracing::info!(
                "[{}] Tunnel client '{}' registered, waiting for disconnect...",
                id,
                register_req.client_id
            );

            while let Ok(_stream) = mux_conn.accept_stream().await {
                tracing::warn!("[{}] Unexpected stream in tunnel mode, discarding", id);
            }

            tracing::info!(
                "[{}] Tunnel client '{}' disconnected, cleaning up",
                id,
                register_req.client_id
            );
            let mut reg = registry.lock().await;
            let no_connections = reg.remove_connection(&register_req.client_id, id);
            if no_connections {
                let empty_ports = reg.remove_client_routes(&register_req.client_id);
                for port in empty_ports {
                    if let Some(port_state) = reg.ports.remove(&port) {
                        port_state.cancel_token.cancel();
                        tracing::info!("Closed listener on port {}", port);
                    }
                }
            }
            Ok(())
        }
    }
}
