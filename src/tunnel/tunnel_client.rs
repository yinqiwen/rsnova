use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

/// Monotonic counter used to derive unique jitter per tunnel connection attempt.
static TUNNEL_CONN_SEED: AtomicU64 = AtomicU64::new(0);

use crate::app_config::AppConfig;
use crate::mux::event::{
    self, AuthAck, AuthRequest, OpenStreamEvent, RegisterAck, RegisterRequest, TunnelEntry,
    FLAG_AUTH_ACK, FLAG_REVERSE_OPEN,
};
use crate::tunnel::client::MuxConnection;
use crate::tunnel::stream::Stream;
use crate::tunnel::tls_client::TlsConnection;

/// Entry point for tunnel client mode (TLS) with hot-reload support.
pub async fn start_tunnel_client_tls(
    url: &Url,
    cert_path: &Path,
    host: &str,
    idle_timeout_secs: usize,
    stream_window: u32,
    app_config: Arc<AppConfig>,
    max_age_secs: u64,
) -> Result<()> {
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
            ) => r,
            _ = token.cancelled() => {
                tracing::info!("Config reloaded, reconnecting TLS tunnel with new entries...");
                backoff_secs = INITIAL_BACKOFF_SECS;
                continue;
            }
        };

        // Reset backoff if connection was productive (lasted > 30s)
        if start.elapsed() > Duration::from_secs(30) {
            backoff_secs = INITIAL_BACKOFF_SECS;
        }

        tracing::info!(
            "Tunnel connection lost ({}), reconnecting in {}s...",
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

async fn run_tunnel_connection_tls(
    url: &Url,
    cert_path: &Path,
    host: &str,
    stream_window: u32,
    client_id: &str,
    entries: &[TunnelEntry],
    idle_timeout_secs: usize,
    max_age_secs: u64,
) -> Result<()> {
    let mut conn = TlsConnection::new(stream_window);
    conn.connect(url, cert_path, host).await?;
    tracing::info!("TLS tunnel connection established");

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

    tracing::info!("Tunnel client ready, waiting for reverse streams...");
    let seed = TUNNEL_CONN_SEED.fetch_add(1, Ordering::Relaxed) as usize;
    let jitter = ((seed.wrapping_mul(73)) % 201) as i64 - 100;
    let retire_at = Instant::now() + Duration::from_secs((max_age_secs as i64 + jitter).max(0) as u64);
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    loop {
        handles.retain(|h| !h.is_finished());

        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(retire_at)) => {
                tracing::info!("TLS tunnel connection reached max age, draining...");
                for h in handles {
                    let _ = h.await;
                }
                return Ok(());
            }
            result = conn.accept_stream() => {
                match result {
                    Ok((mut stream_send, mut stream_recv)) => {
                        handles.push(tokio::spawn(async move {
                            if let Err(e) =
                                handle_reverse_stream(&mut stream_recv, &mut stream_send, idle_timeout_secs).await
                            {
                                tracing::warn!("Reverse stream error: {}", e);
                            }
                        }));
                    }
                    Err(e) => {
                        tracing::error!("accept_stream failed: {}, draining handles", e);
                        for h in handles {
                            let _ = h.await;
                        }
                        return Err(e);
                    }
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
