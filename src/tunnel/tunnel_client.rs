use anyhow::{Result, anyhow};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use url::Url;

/// Monotonic counter used to derive unique jitter per tunnel connection attempt.
static TUNNEL_CONN_SEED: AtomicU64 = AtomicU64::new(0);
pub const MAX_TUNNEL_REVERSE_STREAMS: usize = 256;

use crate::app_config::AppConfig;
use crate::mux::event::{
    self, AuthAck, AuthRequest, FLAG_AUTH_ACK, FLAG_REVERSE_OPEN, OpenStreamEvent, RegisterAck,
    RegisterRequest, TunnelEntry,
};
use crate::tunnel::client::MuxConnection;
use crate::tunnel::stream::Stream;
use crate::tunnel::tls_client::TlsConnection;

/// Entry point for tunnel client mode (TLS) with hot-reload support.
/// Spawns `concurrent` independent tunnel client tasks, each with its own
/// connection, reconnection loop, and authentication.
#[allow(clippy::too_many_arguments)]
pub async fn start_tunnel_client_tls(
    url: &Url,
    cert_path: &Path,
    host: &str,
    idle_timeout_secs: usize,
    stream_window: u32,
    app_config: Arc<AppConfig>,
    max_age_secs: u64,
    concurrent: usize,
) -> Result<()> {
    if concurrent == 0 {
        tracing::warn!("--concurrent is 0, defaulting to 1");
    }
    let concurrent = concurrent.max(1);
    let mut handles = Vec::with_capacity(concurrent);
    for i in 0..concurrent {
        let url = url.clone();
        let cert_path = cert_path.to_path_buf();
        let host = host.to_string();
        let app_config = app_config.clone();
        handles.push(tokio::spawn(async move {
            tunnel_client_loop_tls(
                &url,
                &cert_path,
                &host,
                idle_timeout_secs,
                stream_window,
                i,
                app_config,
                max_age_secs,
            )
            .await;
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

/// Per-connection reconnection loop with exponential backoff and config reload.
#[allow(clippy::too_many_arguments)]
async fn tunnel_client_loop_tls(
    url: &Url,
    cert_path: &Path,
    host: &str,
    idle_timeout_secs: usize,
    stream_window: u32,
    conn_index: usize,
    app_config: Arc<AppConfig>,
    max_age_secs: u64,
) {
    const INITIAL_BACKOFF_SECS: u64 = 1;
    const MAX_BACKOFF_SECS: u64 = 60;

    let mut backoff_secs = INITIAL_BACKOFF_SECS;
    loop {
        let (client_id, entries) = {
            let cfg = app_config.reloadable.lock().await;
            (cfg.tunnel_client_id.clone(), cfg.tunnel_entries.clone())
        };

        let start = Instant::now();
        let token = app_config.reload_token_clone().await;

        let result = tokio::select! {
            r = run_tunnel_connection_tls(
                url,
                cert_path,
                host,
                stream_window,
                &client_id,
                &entries,
                idle_timeout_secs,
                max_age_secs,
                conn_index,
            ) => r,
            _ = token.cancelled() => {
                tracing::info!("[conn-{}] Config reloaded, reconnecting TLS tunnel with new entries...", conn_index);
                backoff_secs = INITIAL_BACKOFF_SECS;
                continue;
            }
        };

        // Reset backoff if connection was productive (lasted > 30s)
        if start.elapsed() > Duration::from_secs(30) {
            backoff_secs = INITIAL_BACKOFF_SECS;
        }

        tracing::info!(
            "[conn-{}] Tunnel connection lost ({}), reconnecting in {}s...",
            conn_index,
            result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            backoff_secs
        );
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_tunnel_connection_tls(
    url: &Url,
    cert_path: &Path,
    host: &str,
    stream_window: u32,
    client_id: &str,
    entries: &[TunnelEntry],
    idle_timeout_secs: usize,
    max_age_secs: u64,
    conn_index: usize,
) -> Result<()> {
    let mut conn = TlsConnection::new(stream_window);
    conn.connect(url, cert_path, host).await?;
    tracing::info!("[conn-{}] TLS tunnel connection established", conn_index);

    let (mut send, mut recv) = conn.open_stream().await?;
    let auth_req = AuthRequest::Register(RegisterRequest {
        client_id: client_id.to_string(),
        tunnels: entries.to_vec(),
    });
    let ev = event::new_auth_event(0, &auth_req)?;
    event::write_event(&mut send, ev).await?;

    let ack_ev = event::read_event(&mut recv).await?;
    if ack_ev.header.flags() != FLAG_AUTH_ACK {
        return Err(anyhow!(
            "expected FLAG_AUTH_ACK, got flag={}",
            ack_ev.header.flags()
        ));
    }
    let config = bincode::config::standard();
    let (ack, _): (AuthAck, usize) = bincode::decode_from_slice(ack_ev.body.as_ref(), config)
        .map_err(|e| anyhow!("decode AuthAck failed: {}", e))?;
    match ack {
        AuthAck::Proxy => return Err(anyhow!("server returned Proxy ack for tunnel request")),
        AuthAck::RegisterAck(register_ack) => {
            handle_register_ack(&register_ack)?;
        }
    }
    drop(send);
    drop(recv);

    tracing::info!(
        "[conn-{}] Tunnel client ready, waiting for reverse streams...",
        conn_index
    );
    let semaphore = Arc::new(Semaphore::new(MAX_TUNNEL_REVERSE_STREAMS));

    let spawn_reverse =
        |mut stream_send, mut stream_recv, semaphore: Arc<Semaphore>| match semaphore
            .try_acquire_owned()
        {
            Ok(permit) => {
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) =
                        handle_reverse_stream(&mut stream_recv, &mut stream_send, idle_timeout_secs)
                            .await
                    {
                        tracing::warn!("Reverse stream error: {}", e);
                    }
                });
            }
            Err(_) => {
                metrics::counter!("tunnel_reverse_streams_rejected").increment(1);
                tracing::warn!(
                    "[conn-{}] max tunnel reverse streams ({}) reached, dropping stream",
                    conn_index,
                    MAX_TUNNEL_REVERSE_STREAMS
                );
            }
        };

    if max_age_secs > 0 {
        let seed = next_tunnel_conn_seed() as usize;
        let jitter = crate::tunnel::client::retirement_jitter_secs(seed);
        let retire_at =
            Instant::now() + Duration::from_secs((max_age_secs as i64 + jitter).max(0) as u64);

        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(retire_at)) => {
                    tracing::info!("[conn-{}] TLS tunnel connection reached max age, stop accepting new streams", conn_index);
                    return Ok(());
                }
                result = conn.accept_stream() => {
                    match result {
                        Ok((stream_send, stream_recv)) => {
                            spawn_reverse(stream_send, stream_recv, semaphore.clone());
                        }
                        Err(e) => {
                            tracing::error!("[conn-{}] accept_stream failed: {}", conn_index, e);
                            return Err(e);
                        }
                    }
                }
            }
        }
    } else {
        loop {
            match conn.accept_stream().await {
                Ok((stream_send, stream_recv)) => {
                    spawn_reverse(stream_send, stream_recv, semaphore.clone());
                }
                Err(e) => {
                    tracing::error!("[conn-{}] accept_stream failed: {}", conn_index, e);
                    return Err(e);
                }
            }
        }
    }
}

pub async fn handle_reverse_stream<
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
>(
    recv: &mut R,
    send: &mut W,
    idle_timeout_secs: usize,
) -> Result<()> {
    let ev = event::read_event(recv).await?;
    if ev.header.flags() != FLAG_REVERSE_OPEN {
        return Err(anyhow!(
            "expected FLAG_REVERSE_OPEN, got flag={}",
            ev.header.flags()
        ));
    }
    let config = bincode::config::standard();
    let (open_event, _): (OpenStreamEvent, usize) =
        bincode::decode_from_slice(ev.body.as_ref(), config)
            .map_err(|e| anyhow!("decode OpenStreamEvent failed: {}", e))?;

    tracing::info!("Reverse stream: connecting to {}", open_event.addr);

    let timeout_dur = Duration::from_secs(30);
    let local_stream = tokio::time::timeout(
        timeout_dur,
        tokio::net::TcpStream::connect(&open_event.addr),
    )
    .await
    .map_err(|_| anyhow!("connect to {} timed out", open_event.addr))?
    .map_err(|e| anyhow!("connect to {} failed: {}", open_event.addr, e))?;

    let (mut local_r, mut local_w) = local_stream.into_split();
    let mut stream = Stream::new(&mut local_r, &mut local_w, recv, send);
    stream.transfer(idle_timeout_secs).await?;
    Ok(())
}

/// Shared handler for RegisterAck — used by both TLS and QUIC tunnel clients.
pub fn handle_register_ack(ack: &RegisterAck) -> Result<()> {
    let mut any_success = false;
    for result in &ack.results {
        if result.success {
            any_success = true;
            tracing::info!(
                "Tunnel registered: :{}{} → OK",
                result.remote_port,
                result
                    .sni
                    .as_ref()
                    .map(|s| format!(" (SNI: {})", s))
                    .unwrap_or_default()
            );
        } else {
            tracing::error!(
                "Tunnel registration failed: :{}{} — {}",
                result.remote_port,
                result
                    .sni
                    .as_ref()
                    .map(|s| format!(" (SNI: {})", s))
                    .unwrap_or_default(),
                result.error.as_deref().unwrap_or("unknown error")
            );
        }
    }
    if !any_success {
        Err(anyhow!("all tunnel registrations failed"))
    } else {
        Ok(())
    }
}

/// Returns a monotonically increasing seed for tunnel connection jitter.
pub fn next_tunnel_conn_seed() -> u64 {
    TUNNEL_CONN_SEED.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::event::{RegisterAck, TunnelResult};

    /// When every registration fails, `handle_register_ack` returns an error
    /// whose text is the aggregate "all tunnel registrations failed" — NOT a
    /// generic stream error like "close by remote". The server previously
    /// dropped its mux::Connection right after writing the (unflushed) failure
    /// ACK, racing the teardown so the client sometimes lost the ACK and
    /// reported "close by remote" instead of reaching `handle_register_ack`
    /// at all. This test pins the client-side contract: with the server flush
    /// fix in place, the client reliably decodes the RegisterAck and surfaces
    /// the aggregate registration-failure error rather than a stream teardown.
    #[test]
    fn handle_register_ack_surfaces_registration_failure_not_close_by_remote() {
        let ack = RegisterAck {
            results: vec![TunnelResult {
                success: false,
                remote_port: 15721,
                sni: None,
                error: Some("tunnel not enabled on server".to_string()),
            }],
        };
        let err = handle_register_ack(&ack).expect_err("all-failed ack must error");
        let msg = err.to_string();
        assert!(
            msg.contains("registration"),
            "expected an aggregate registration-failure error, got: {}",
            msg
        );
        assert!(
            !msg.contains("close by remote"),
            "must not surface a stream-teardown error"
        );
    }

    /// A single successful registration is enough for the client to proceed —
    /// failures among other entries are logged but do not block readiness.
    #[test]
    fn handle_register_ack_ok_when_any_entry_succeeds() {
        let ack = RegisterAck {
            results: vec![
                TunnelResult {
                    success: false,
                    remote_port: 80,
                    sni: None,
                    error: Some("bind failed".to_string()),
                },
                TunnelResult {
                    success: true,
                    remote_port: 15721,
                    sni: None,
                    error: None,
                },
            ],
        };
        assert!(handle_register_ack(&ack).is_ok());
    }
}
