// use anyhow::Context;
use anyhow::{Result, anyhow};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Semaphore;
use tokio::time::Duration;

use crate::mux::event;
use crate::tunnel::stream::handle_server_stream;
use crate::{mux, tunnel::ALPN_QUIC_HTTP};

use std::{collections::VecDeque, net::SocketAddr, path::Path, sync::Mutex};

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::utils::read_private_key;
use crate::utils::read_tokio_tls_certs;

/// Maximum concurrent outbound TCP connections per TLS client connection.
/// Limits fd consumption under load and prevents EMFILE cascading failures.
const MAX_SERVER_PROXY_STREAMS: usize = 256;

/// Maximum concurrent inbound TLS client connections the server will hold.
/// Each accepted connection spawns a mux dispatcher task plus a 256-entry
/// control channel (~25KB resident), so an unbounded accept loop grows
/// memory linearly under port scans or connection churn. 1024 matches the
/// per-connection mux stream cap and keeps worst-case dispatcher memory
/// around ~25MB; beyond that new connections are refused at accept time.
const MAX_SERVER_CONNECTIONS: usize = 1024;
const AUTH_TIMEOUT: Duration = Duration::from_secs(15);

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
    let conn_semaphore = Arc::new(Semaphore::new(MAX_SERVER_CONNECTIONS));
    let mut accept_backoff = crate::utils::AcceptBackoff::default();
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => {
                accept_backoff.reset();
                accepted
            }
            Err(error) => {
                let delay = accept_backoff.next_delay();
                metrics::counter!("tls_server_accept_retries").increment(1);
                tracing::warn!("TLS accept failed: {}; retrying in {:?}", error, delay);
                tokio::time::sleep(delay).await;
                continue;
            }
        };
        crate::utils::set_tcp_keepalive(&stream);
        // Reject excess connections before allocating a dispatcher + control
        // channel for them. The permit is held by the connection task and
        // released when it exits, so slots recycle as connections close.
        let permit = match conn_semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                metrics::counter!("tls_server_connections_rejected").increment(1);
                tracing::warn!(
                    "max TLS connections ({}) reached, dropping connection",
                    MAX_SERVER_CONNECTIONS
                );
                // `stream` is dropped here, closing the socket immediately.
                continue;
            }
        };
        let conn_id = if free_ids.lock().unwrap().is_empty() {
            id += 1;
            id - 1
        } else {
            free_ids.lock().unwrap().pop_front().unwrap()
        };
        metrics::gauge!("tls_server_connections").increment(1.0);
        let acceptor = acceptor.clone();
        let fut_free_ids = free_ids.clone();
        let registry = registry.clone();
        let fut = async move {
            let auth_deadline = tokio::time::Instant::now() + AUTH_TIMEOUT;
            let stream = tokio::time::timeout_at(auth_deadline, acceptor.accept(stream))
                .await
                .map_err(|_| anyhow!("TLS handshake timed out"))??;
            tracing::info!("TLS connection incoming");
            handle_tls_connection_until(
                stream,
                conn_id,
                idle_timeout_secs,
                stream_window,
                registry,
                auth_deadline,
            )
            .await?;
            Ok(()) as Result<()>
        };

        tokio::spawn(async move {
            let _permit = permit; // held until this task exits
            if let Err(e) = fut.await {
                tracing::error!("connection failed: {reason}", reason = e.to_string())
            }
            metrics::gauge!("tls_server_connections").decrement(1.0);
            fut_free_ids.lock().unwrap().push_back(conn_id);
        });
    }
}

#[cfg(test)]
pub(crate) async fn handle_tls_connection<T: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    conn: T,
    id: u32,
    idle_timeout_secs: usize,
    stream_window: u32,
    registry: Option<crate::tunnel::tunnel_registry::SharedRegistry>,
) -> Result<()> {
    handle_tls_connection_until(
        conn,
        id,
        idle_timeout_secs,
        stream_window,
        registry,
        tokio::time::Instant::now() + AUTH_TIMEOUT,
    )
    .await
}

async fn handle_tls_connection_until<T: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    conn: T,
    id: u32,
    idle_timeout_secs: usize,
    stream_window: u32,
    registry: Option<crate::tunnel::tunnel_registry::SharedRegistry>,
    auth_deadline: tokio::time::Instant,
) -> Result<()> {
    let (r, w) = tokio::io::split(conn);
    let mux_conn = Arc::new(mux::Connection::new_with_stream_window(
        r,
        w,
        mux::Mode::Server,
        id,
        stream_window,
    ));

    let auth_stream = tokio::time::timeout_at(auth_deadline, mux_conn.accept_stream())
        .await
        .map_err(|_| {
            metrics::counter!("tls_server_auth_timeouts").increment(1);
            anyhow!("authentication timed out waiting for auth stream")
        })??;
    let (mut auth_r, mut auth_w) = tokio::io::split(auth_stream);

    let ev = tokio::time::timeout_at(auth_deadline, event::read_event(&mut auth_r))
        .await
        .map_err(|_| {
            metrics::counter!("tls_server_auth_timeouts").increment(1);
            anyhow!("authentication timed out waiting for auth event")
        })??;
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
            tokio::io::AsyncWriteExt::flush(&mut auth_w).await?;
            drop(auth_r);
            drop(auth_w);

            let semaphore = Arc::new(Semaphore::new(MAX_SERVER_PROXY_STREAMS));
            // Shared counter for EMFILE detection across all spawned tasks.
            // When a task detects EMFILE, it increments this counter; the
            // accept loop reads and resets it to apply backpressure.
            let emfile_count = Arc::new(std::sync::atomic::AtomicU32::new(0));

            loop {
                let stream = match mux_conn.accept_stream().await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("[{}] accept_stream failed: {}", id, e);
                        return Ok(());
                    }
                };

                // Backpressure: if recent streams hit EMFILE, pause before
                // accepting more to avoid a tight retry loop that floods
                // the system with doomed TcpStream::connect attempts.
                let recent_emfile = emfile_count.swap(0, std::sync::atomic::Ordering::Relaxed);
                if recent_emfile > 0 {
                    let backoff = Duration::from_millis(100 * recent_emfile.min(10) as u64);
                    tracing::warn!(
                        "[{}] EMFILE backpressure ({} recent), sleeping {:?}",
                        id,
                        recent_emfile,
                        backoff,
                    );
                    tokio::time::sleep(backoff).await;
                }

                let permit = match semaphore.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        tracing::warn!(
                            "[{}] max concurrent proxy streams ({}) reached, dropping stream {}",
                            id,
                            MAX_SERVER_PROXY_STREAMS,
                            stream.id(),
                        );
                        // Stream is dropped — MuxStream::Drop sends StreamClose
                        // to the dispatcher, which sends FIN to the client so it
                        // knows the stream was rejected rather than silently lost.
                        metrics::counter!("tls_server_proxy_streams_rejected").increment(1);
                        continue;
                    }
                };

                metrics::gauge!("tls_server_proxy_streams").increment(1.0);
                let conn_id = id;
                let emfile_ref = emfile_count.clone();

                tokio::spawn(async move {
                    let stream_id = stream.id();
                    let _permit = permit; // hold permit until task completes
                    let (mut stream_reader, mut stream_writer) = tokio::io::split(stream);
                    if let Err(e) = handle_server_stream(
                        &mut stream_reader,
                        &mut stream_writer,
                        idle_timeout_secs,
                    )
                    .await
                    {
                        let err_str = e.to_string();
                        if err_str.contains("file descriptor") || err_str.contains("os error 24") {
                            emfile_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            metrics::counter!("tls_server_emfile").increment(1);
                        }
                        tracing::error!(
                            "[{}/{}]failed: {reason}",
                            conn_id,
                            stream_id,
                            reason = err_str
                        );
                    }
                    metrics::gauge!("tls_server_proxy_streams").decrement(1.0);
                });
            }
        }
        event::AuthRequest::Register(register_req) => {
            // On the success path we carry the SharedRegistry Arc out of the
            // match in `register_req` so we can lock it for cleanup after the
            // accept loop exits. On the reject path (`registry` is None,
            // reachable only when this server was started without tunnel
            // support) `register_req` stays None and there is nothing to clean.
            let (ack, register_req) = match registry {
                Some(registry) => {
                    let results = crate::tunnel::tunnel_remote::handle_tunnel_register(
                        &registry,
                        &register_req,
                        crate::tunnel::tunnel_registry::ConnectionHandler::Tls(mux_conn.clone()),
                        id,
                        idle_timeout_secs,
                    )
                    .await;

                    let ack = event::AuthAck::RegisterAck(event::RegisterAck { results });
                    (ack, Some((register_req, registry)))
                }
                // Tunnel not enabled on server: reject every entry. We still
                // send the failure RegisterAck so the client learns the real
                // reason rather than observing a silent disconnect.
                None => {
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
                    (ack, None)
                }
            };

            let ack_ev = event::new_auth_ack_event(0, &ack)?;
            event::write_event(&mut auth_w, ack_ev).await?;
            // We do NOT return immediately after writing the ACK. This function
            // owns the last `Arc<mux::Connection>` ref, so returning drops it —
            // and `mux::Connection::Drop` aborts the dispatcher task, which can
            // happen before the dispatcher has drained the queued ACK from the
            // control channel and written it to the socket. The client would
            // then observe a torn-down connection ("close by remote") instead
            // of the real registration result. Entering the accept loop below
            // keeps `mux_conn` alive so the dispatcher flushes the ACK; the
            // connection tears down naturally when the client disconnects.
            match &register_req {
                Some((req, _)) => tracing::info!(
                    "[{}] Tunnel client '{}' registered, waiting for disconnect...",
                    id,
                    req.client_id
                ),
                None => tracing::info!(
                    "[{}] Tunnel registration rejected (tunnel not enabled), waiting for disconnect...",
                    id,
                ),
            }

            let mut control_task =
                tokio::spawn(async move { event::read_event(&mut auth_r).await });
            let mut draining = false;
            loop {
                tokio::select! {
                    control = &mut control_task, if !draining => {
                        match control {
                            Ok(Ok(ev)) if ev.header.flags() == event::FLAG_DRAIN => {
                                draining = true;
                                if let Some((req, registry)) = &register_req {
                                    let marked = registry.lock().await.begin_drain(&req.client_id, id);
                                    tracing::info!(
                                        "[{}] Tunnel client '{}' draining (registered={})",
                                        id,
                                        req.client_id,
                                        marked,
                                    );
                                }
                            }
                            Ok(Ok(ev)) => {
                                tracing::warn!(
                                    "[{}] unexpected tunnel control flag {}, closing generation",
                                    id,
                                    ev.header.flags(),
                                );
                                break;
                            }
                            Ok(Err(e)) => {
                                tracing::debug!("[{}] tunnel control stream closed: {}", id, e);
                                break;
                            }
                            Err(e) => {
                                tracing::debug!("[{}] tunnel control task failed: {}", id, e);
                                break;
                            }
                        }
                    }
                    stream = mux_conn.accept_stream() => {
                        match stream {
                            Ok(_stream) => {
                                tracing::warn!("[{}] Unexpected stream in tunnel mode, discarding", id);
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
            control_task.abort();
            drop(auth_w);

            if let Some((req, registry)) = register_req {
                tracing::info!(
                    "[{}] Tunnel client '{}' disconnected, cleaning up",
                    id,
                    req.client_id
                );
                let mut reg = registry.lock().await;
                let no_connections = reg.remove_connection(&req.client_id, id);
                if no_connections {
                    let empty_ports = reg.remove_client_routes(&req.client_id);
                    for port in empty_ports {
                        if let Some(port_state) = reg.ports.remove(&port) {
                            port_state.cancel_token.cancel();
                            tracing::info!("Closed listener on port {}", port);
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::event::TunnelEntry;
    use crate::mux::{self, event};

    #[tokio::test]
    async fn silent_client_hits_auth_deadline_and_releases_permit() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let (client, server) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move {
            let _permit = permit;
            let _client = client;
            handle_tls_connection_until(
                server,
                0,
                0,
                mux::INITIAL_STREAM_WINDOW,
                None,
                tokio::time::Instant::now() + Duration::from_millis(20),
            )
            .await
        });

        let error = task
            .await
            .unwrap()
            .expect_err("silent client must time out");
        assert!(error.to_string().contains("timed out"));
        assert_eq!(semaphore.available_permits(), 1);
    }

    /// Regression test for the "close by remote" race.
    ///
    /// When the server rejects tunnel registration (here: `registry` is None,
    /// simulating "tunnel not enabled on server" before the default-port-range
    /// fix), `handle_tls_connection` writes a failure RegisterAck and returns,
    /// dropping the last `Arc<mux::Connection>` — whose `Drop` aborts the
    /// dispatcher and tears down the socket. Before the flush fix, the ACK sat
    /// in the write buffer and was lost on teardown, so the client's
    /// `read_event` hit a stream-closed error ("close by remote") instead of
    /// reading the real failure reason.
    ///
    /// This wires a client mux::Connection to a server `handle_tls_connection`
    /// over `tokio::io::duplex`, drives a Register handshake, and asserts the
    /// client receives FLAG_AUTH_ACK with the server's error text — not a
    /// stream-closed error.
    #[tokio::test]
    async fn server_flushes_failure_ack_before_tearing_down_connection() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_r, client_w) = tokio::io::split(client);

        // Client-side mux connection (even stream ids).
        let client_conn = mux::Connection::new_with_stream_window(
            client_r,
            client_w,
            mux::Mode::Client,
            0,
            mux::INITIAL_STREAM_WINDOW,
        );

        // Drive the server handler to completion in a separate task. Passing
        // registry=None exercises the "tunnel not enabled" reject path that
        // returns Ok(()) right after writing the failure ACK. handle_tls_connection
        // owns the full duplex stream and splits it internally.
        let server_task = tokio::spawn(async move {
            handle_tls_connection(server, 0, 120, mux::INITIAL_STREAM_WINDOW, None).await
        });

        // Client opens the auth stream and sends a Register request.
        let mut auth_stream = client_conn.open_stream().await.expect("open_stream");
        let auth_req = event::AuthRequest::Register(event::RegisterRequest {
            client_id: "test-client".to_string(),
            tunnels: vec![TunnelEntry {
                local_addr: "localhost:15721".to_string(),
                remote_port: 15721,
                sni: None,
            }],
        });
        let ev = event::new_auth_event(0, &auth_req).expect("encode auth");
        event::write_event(&mut auth_stream, ev)
            .await
            .expect("write auth");

        // The crux of the regression: this read must succeed with the ACK,
        // not fail with a stream-closed / "close by remote" error.
        let ack_ev = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            event::read_event(&mut auth_stream),
        )
        .await
        .expect("client read timed out — ACK likely lost on connection teardown")
        .expect("client read errored — server likely tore down before flushing the ACK");

        assert_eq!(
            ack_ev.header.flags(),
            event::FLAG_AUTH_ACK,
            "expected FLAG_AUTH_ACK, got flag={}",
            ack_ev.header.flags()
        );

        let config = bincode::config::standard();
        let (ack, _): (event::AuthAck, usize) =
            bincode::decode_from_slice(ack_ev.body.as_ref(), config).expect("decode AuthAck");
        match ack {
            event::AuthAck::RegisterAck(register_ack) => {
                assert_eq!(register_ack.results.len(), 1);
                let r = &register_ack.results[0];
                assert!(!r.success, "server should have rejected the tunnel");
                assert_eq!(r.remote_port, 15721);
                assert!(
                    r.error
                        .as_deref()
                        .unwrap_or_default()
                        .contains("tunnel not enabled on server"),
                    "expected server error text, got: {:?}",
                    r.error
                );
            }
            event::AuthAck::Proxy => panic!("expected RegisterAck, got Proxy"),
        }

        // Now that the client has the ACK, close the client side so the
        // server's accept_stream loop sees the disconnect and returns. Without
        // this, the server (in the reject path) stays in its accept loop
        // forever and `server_task` never completes.
        drop(auth_stream);
        drop(client_conn);

        // Server must exit its accept loop and return Ok once the client has
        // gone away. Bounded: if the server fails to notice the disconnect the
        // test fails fast rather than hanging.
        tokio::time::timeout(std::time::Duration::from_secs(5), server_task)
            .await
            .expect("server did not return after client disconnect — accept loop stuck")
            .expect("server task panicked")
            .expect("server handler returned an error");
    }
}
