use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::mux::event::{self, OpenStreamEvent, RegisterRequest, StreamProto, TunnelResult};
use crate::tunnel::stream::Stream;
use crate::tunnel::tls_local::peek_sni_v2;
use crate::tunnel::tunnel_registry::{
    ClientConnection, ConnectionHandler, PortState, SharedRegistry,
};

const MAX_VISITORS_PER_PORT: usize = 1024;
const SNI_PEEK_TIMEOUT: Duration = Duration::from_millis(500);

/// Handle tunnel registration: validate entries, bind ports, store connection.
pub async fn handle_tunnel_register(
    registry: &SharedRegistry,
    req: &RegisterRequest,
    handler: ConnectionHandler,
    conn_id: u32,
    idle_timeout_secs: usize,
) -> Vec<TunnelResult> {
    let mut results = Vec::new();
    let mut successful_entries = Vec::new();
    let mut reg = registry.lock().await;

    for entry in &req.tunnels {
        if let Some(err) = reg.validate_entry(&req.client_id, entry) {
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
        successful_entries.push(entry.clone());
        results.push(TunnelResult {
            success: true,
            remote_port: entry.remote_port,
            sni: entry.sni.clone(),
            error: None,
        });
    }

    if !successful_entries.is_empty() {
        let empty_ports = reg.reconcile_client_routes(&req.client_id, &successful_entries);
        for port in empty_ports {
            if let Some(port_state) = reg.ports.remove(&port) {
                port_state.cancel_token.cancel();
            }
        }
        reg.add_connection(
            &req.client_id,
            ClientConnection {
                handler,
                conn_id,
                draining: false,
                consecutive_failures: 0,
                slot: 0, // assigned by add_connection
            },
        );
    }

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
        let mut accept_backoff = crate::utils::AcceptBackoff::default();
        let visitor_semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_VISITORS_PER_PORT));
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("Visitor accept loop for port {} cancelled", port);
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            accept_backoff.reset();
                            let permit = match visitor_semaphore.clone().try_acquire_owned() {
                                Ok(permit) => permit,
                                Err(_) => {
                                    metrics::counter!("tunnel_visitors_rejected").increment(1);
                                    tracing::warn!(
                                        "max visitors ({}) reached on port {}, rejecting {}",
                                        MAX_VISITORS_PER_PORT,
                                        port,
                                        addr,
                                    );
                                    continue;
                                }
                            };
                            tracing::debug!("Visitor connection from {} on port {}", addr, port);
                            let registry = registry.clone();
                            tokio::spawn(async move {
                                let _permit = permit;
                                if let Err(e) = handle_visitor(port, stream, registry, idle_timeout_secs).await {
                                    tracing::warn!("Visitor handler error on port {}: {}", port, e);
                                }
                            });
                        }
                        Err(e) => {
                            let delay = accept_backoff.next_delay();
                            metrics::counter!("tunnel_visitor_accept_retries").increment(1);
                            tracing::warn!(
                                "Accept error on port {}: {}; retrying in {:?}",
                                port,
                                e,
                                delay,
                            );
                            tokio::select! {
                                _ = cancel_token.cancelled() => break,
                                _ = tokio::time::sleep(delay) => {}
                            }
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
    let needs_sni = registry.lock().await.port_has_sni_routes(port);
    let sni = if needs_sni {
        peek_sni_bounded(&visitor_stream, SNI_PEEK_TIMEOUT)
            .await
            .ok()
    } else {
        None
    };

    // Route lookup + handler selection, with one self-healing retry.
    //
    // The retry covers the "tunnel black hole" race: connection A of a client
    // disconnects and wipes the client's routes while connection B is still
    // alive. The next visitor finds no route and would previously have been
    // dropped until B's own reconnect re-registered. Since B (or a fresh C)
    // typically re-registers within its backoff interval, a short wait + one
    // re-lookup turns that multi-second visitor outage into a single delayed
    // request. `remove_client_routes` also keeps `last_entries`, so a fully
    // wiped client with a live connection can be re-registered on the spot.
    let mut attempt: u32 = 0;
    let (local_addr, client_id, handler, handler_slot) = loop {
        attempt += 1;
        let mut reg = registry.lock().await;

        if reg.lookup_route(port, sni.as_deref()).is_none() {
            // Try to heal: any client with a live connection and remembered
            // entries gets its routes re-registered.
            let candidate_ids: Vec<String> = reg
                .clients
                .iter()
                .filter(|(_, c)| !c.connections.is_empty() && !c.last_entries.is_empty())
                .map(|(id, _)| id.clone())
                .collect();
            for cid in candidate_ids {
                let entries = reg.last_entries_of(&cid);
                for entry in &entries {
                    if entry.remote_port == port {
                        tracing::info!(
                            "re-registering wiped route :{} for client '{}' (self-heal)",
                            port,
                            cid
                        );
                        reg.register_route(&cid, entry);
                    }
                }
            }
        }

        match reg.lookup_route(port, sni.as_deref()) {
            Some(tunnel) => {
                let local_addr = tunnel.local_addr.clone();
                let client_id = tunnel.client_id.clone();
                let client_state = reg
                    .clients
                    .get_mut(&client_id)
                    .ok_or_else(|| anyhow!("client '{}' not found in registry", client_id))?;
                match client_state.next_connection() {
                    Some((handler, slot)) => break (local_addr, client_id, handler, slot),
                    None if attempt < 2 => {
                        drop(reg);
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                    None => {
                        return Err(anyhow!("client '{}' has no active connections", client_id));
                    }
                }
            }
            None if attempt < 2 => {
                drop(reg);
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            None => {
                return Err(anyhow!(
                    "no route for port {}:{}",
                    port,
                    sni.as_deref().unwrap_or("default")
                ));
            }
        }
    };

    match handler {
        ConnectionHandler::Tls(mux_conn) => {
            let mux_stream = match mux_conn.open_stream().await {
                Ok(s) => {
                    registry
                        .lock()
                        .await
                        .record_open_result(&client_id, handler_slot, true);
                    s
                }
                Err(e) => {
                    // Feed the consecutive-failure counter so `next_connection`
                    // drops this handler once it proves dead, instead of
                    // black-holing every subsequent visitor.
                    registry
                        .lock()
                        .await
                        .record_open_result(&client_id, handler_slot, false);
                    return Err(anyhow!("open reverse stream failed: {}", e));
                }
            };

            let open_ev = OpenStreamEvent {
                proto: StreamProto::Tcp,
                addr: local_addr,
            };
            let ev = event::new_reverse_open_stream_event(0, &open_ev)?;
            let (mut stream_r, mut stream_w) = tokio::io::split(mux_stream);
            event::write_event(&mut stream_w, ev).await?;

            let (mut visitor_r, mut visitor_w) = visitor_stream.into_split();
            let mut relay =
                Stream::new(&mut visitor_r, &mut visitor_w, &mut stream_r, &mut stream_w);
            relay.transfer(idle_timeout_secs).await?;
        }
        ConnectionHandler::Quic(mut handle) => {
            let stream = match handle.open_bidirectional_stream().await {
                Ok(s) => {
                    registry
                        .lock()
                        .await
                        .record_open_result(&client_id, handler_slot, true);
                    s
                }
                Err(e) => {
                    registry
                        .lock()
                        .await
                        .record_open_result(&client_id, handler_slot, false);
                    return Err(anyhow!("QUIC open_bidirectional_stream failed: {}", e));
                }
            };
            let (mut recv_stream, mut send_stream) = stream.split();

            let open_ev = OpenStreamEvent {
                proto: StreamProto::Tcp,
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

async fn peek_sni_bounded(stream: &tokio::net::TcpStream, timeout: Duration) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, peek_sni_v2(stream)).await {
            Err(_) => return Err(anyhow!("SNI peek timed out")),
            Ok(Ok(sni)) => return Ok(sni),
            Ok(Err(error)) => {
                let message = error.to_string();
                if !message.contains("incomplete") && !message.contains("unexpected end") {
                    return Err(error);
                }
            }
        }
        tokio::time::timeout_at(deadline, tokio::time::sleep(Duration::from_millis(5)))
            .await
            .map_err(|_| anyhow!("SNI peek timed out"))?;
    }
}
