use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::mux::event::{self, OpenStreamEvent, RegisterRequest, TunnelResult};
use crate::tunnel::stream::Stream;
use crate::tunnel::tls_local::peek_sni_v2;
use crate::tunnel::tunnel_registry::{
    ClientConnection, ConnectionHandler, PortState, SharedRegistry,
};

/// Handle tunnel registration: validate entries, bind ports, store connection.
pub async fn handle_tunnel_register(
    registry: &SharedRegistry,
    req: &RegisterRequest,
    handler: ConnectionHandler,
    conn_id: u32,
    idle_timeout_secs: usize,
) -> Vec<TunnelResult> {
    let mut results = Vec::new();
    let mut reg = registry.lock().await;

    for entry in &req.tunnels {
        if let Some(err) = reg.validate_entry(entry) {
            results.push(TunnelResult {
                success: false,
                remote_port: entry.remote_port,
                sni: entry.sni.clone(),
                error: Some(err),
            });
            continue;
        }

        if !reg.has_port(entry.remote_port) {
            let addr = format!("0.0.0.0:{}", entry.remote_port);
            match TcpListener::bind(&addr).await {
                Ok(listener) => {
                    let cancel_token = CancellationToken::new();
                    let listener = Arc::new(listener);

                    let handle = spawn_visitor_accept_loop(
                        entry.remote_port,
                        listener.clone(),
                        registry.clone(),
                        cancel_token.clone(),
                        idle_timeout_secs,
                    );

                    reg.ports.insert(
                        entry.remote_port,
                        PortState {
                            listener,
                            listener_handle: handle,
                            cancel_token,
                            active_routes: Vec::new(),
                        },
                    );
                    tracing::info!("Bound tunnel port: 0.0.0.0:{}", entry.remote_port);
                }
                Err(e) => {
                    results.push(TunnelResult {
                        success: false,
                        remote_port: entry.remote_port,
                        sni: entry.sni.clone(),
                        error: Some(format!("bind failed: {}", e)),
                    });
                    continue;
                }
            }
        }

        reg.register_route(&req.client_id, entry);
        results.push(TunnelResult {
            success: true,
            remote_port: entry.remote_port,
            sni: entry.sni.clone(),
            error: None,
        });
    }

    reg.add_connection(
        &req.client_id,
        ClientConnection { handler, conn_id },
    );

    results
}

fn spawn_visitor_accept_loop(
    port: u16,
    listener: Arc<TcpListener>,
    registry: SharedRegistry,
    cancel_token: CancellationToken,
    idle_timeout_secs: usize,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("Visitor accept loop for port {} cancelled", port);
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            tracing::debug!("Visitor connection from {} on port {}", addr, port);
                            let registry = registry.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_visitor(port, stream, registry, idle_timeout_secs).await {
                                    tracing::warn!("Visitor handler error on port {}: {}", port, e);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Accept error on port {}: {}", port, e);
                            break;
                        }
                    }
                }
            }
        }
    })
}

async fn handle_visitor(
    port: u16,
    visitor_stream: tokio::net::TcpStream,
    registry: SharedRegistry,
    idle_timeout_secs: usize,
) -> Result<()> {
    let sni = match tokio::time::timeout(Duration::from_secs(5), peek_sni_v2(&visitor_stream)).await
    {
        Ok(Ok(sni)) => Some(sni),
        _ => None, // timeout or parse failure → use default route
    };

    let (local_addr, handler) = {
        let mut reg = registry.lock().await;
        let tunnel = reg
            .lookup_route(port, sni.as_deref())
            .ok_or_else(|| {
                anyhow!(
                    "no route for port {}:{}",
                    port,
                    sni.as_deref().unwrap_or("default")
                )
            })?;

        let local_addr = tunnel.local_addr.clone();
        let client_id = tunnel.client_id.clone();

        let client_state = reg
            .clients
            .get_mut(&client_id)
            .ok_or_else(|| anyhow!("client '{}' not found in registry", client_id))?;

        let handler = client_state
            .next_connection()
            .ok_or_else(|| anyhow!("client '{}' has no active connections", client_id))?
            .clone();

        (local_addr, handler)
    };

    match handler {
        ConnectionHandler::Tls(mux_conn) => {
            let mux_stream = mux_conn.open_stream().await?;

            let open_ev = OpenStreamEvent {
                proto: "tcp".to_string(),
                addr: local_addr,
            };
            let ev = event::new_reverse_open_stream_event(0, &open_ev)?;
            let (mut stream_r, mut stream_w) = tokio::io::split(mux_stream);
            event::write_event(&mut stream_w, ev).await?;

            let (mut visitor_r, mut visitor_w) = visitor_stream.into_split();
            let mut relay = Stream::new(&mut visitor_r, &mut visitor_w, &mut stream_r, &mut stream_w);
            relay.transfer(idle_timeout_secs).await?;
        }
        #[cfg(feature = "s2n_quic")]
        ConnectionHandler::Quic(mut handle) => {
            let stream = handle
                .open_bidirectional_stream()
                .await
                .map_err(|e| anyhow!("QUIC open_bidirectional_stream failed: {}", e))?;
            let (mut recv_stream, mut send_stream) = stream.split();

            let open_ev = OpenStreamEvent {
                proto: "tcp".to_string(),
                addr: local_addr,
            };
            let ev = event::new_reverse_open_stream_event(0, &open_ev)?;
            event::write_event(&mut send_stream, ev).await?;

            let (mut visitor_r, mut visitor_w) = visitor_stream.into_split();
            let mut relay = Stream::new(
                &mut visitor_r,
                &mut visitor_w,
                &mut recv_stream,
                &mut send_stream,
            );
            relay.transfer(idle_timeout_secs).await?;
        }
    }

    Ok(())
}
