use anyhow::{Result, anyhow};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Semaphore, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use url::Url;

/// Monotonic counter used to derive unique jitter per tunnel connection attempt.
static TUNNEL_CONN_SEED: AtomicU64 = AtomicU64::new(0);
pub const MAX_TUNNEL_REVERSE_STREAMS: usize = 256;

/// Interval between control-connection pings in tunnel mode. Detects a
/// half-open tunnel control link (NAT silently dropped the session) while it
/// is idle, instead of waiting for TCP keepalive (hours) or the next failed
/// reverse stream. Data flow through active reverse streams short-circuits
/// the ping (see `mux::Connection::ping`), so this costs nothing under load.
const TUNNEL_PING_INTERVAL: Duration = Duration::from_secs(30);

struct TlsGeneration {
    ready: Option<oneshot::Receiver<std::result::Result<(), String>>>,
    drain: Option<oneshot::Sender<()>>,
    join: JoinHandle<Result<()>>,
}

impl TlsGeneration {
    async fn wait_ready(&mut self) -> Result<()> {
        let ready = self
            .ready
            .take()
            .ok_or_else(|| anyhow!("generation readiness already consumed"))?;
        match ready.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(_) => Err(anyhow!("generation exited before registration")),
        }
    }

    fn begin_drain(&mut self) {
        if let Some(drain) = self.drain.take() {
            let _ = drain.send(());
        }
    }
}

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
    let mut current: Option<TlsGeneration> = None;
    let mut current_reload_token: Option<tokio_util::sync::CancellationToken> = None;

    loop {
        if current.is_none() {
            let reload_token = app_config.reload_token_clone().await;
            let (client_id, entries) = current_tunnel_config(&app_config).await;
            let mut candidate = spawn_tls_generation(
                url.clone(),
                cert_path.to_path_buf(),
                host.to_string(),
                stream_window,
                client_id,
                entries,
                idle_timeout_secs,
                conn_index,
            );
            match candidate.wait_ready().await {
                Ok(()) => {
                    backoff_secs = INITIAL_BACKOFF_SECS;
                    current = Some(candidate);
                    current_reload_token = Some(reload_token);
                }
                Err(error) => {
                    tracing::warn!(
                        "[conn-{}] TLS tunnel registration failed: {}; retrying in {}s",
                        conn_index,
                        error,
                        backoff_secs,
                    );
                    let _ = candidate.join.await;
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                    continue;
                }
            }
        }

        let token = current_reload_token
            .as_ref()
            .expect("current generation has reload token")
            .clone();
        let retirement = retirement_delay(max_age_secs);
        tokio::pin!(retirement);
        let current_generation = current.as_mut().expect("current generation exists");
        let replace = tokio::select! {
            result = &mut current_generation.join => {
                tracing::warn!(
                    "[conn-{}] TLS tunnel generation ended: {}",
                    conn_index,
                    join_result_message(result),
                );
                current = None;
                current_reload_token = None;
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                false
            }
            _ = token.cancelled() => {
                tracing::info!("[conn-{}] TLS tunnel config changed; starting replacement", conn_index);
                true
            }
            _ = &mut retirement => {
                tracing::info!("[conn-{}] TLS tunnel reached max age; starting replacement", conn_index);
                true
            }
        };
        if !replace {
            continue;
        }

        let mut replacement_backoff = INITIAL_BACKOFF_SECS;
        loop {
            let replacement_token = app_config.reload_token_clone().await;
            let (client_id, entries) = current_tunnel_config(&app_config).await;
            let mut replacement = spawn_tls_generation(
                url.clone(),
                cert_path.to_path_buf(),
                host.to_string(),
                stream_window,
                client_id,
                entries,
                idle_timeout_secs,
                conn_index,
            );
            match replacement.wait_ready().await {
                Ok(()) => {
                    let mut old = current.take().expect("old generation exists");
                    old.begin_drain();
                    tokio::spawn(async move {
                        let _ = old.join.await;
                    });
                    current = Some(replacement);
                    current_reload_token = Some(replacement_token);
                    backoff_secs = INITIAL_BACKOFF_SECS;
                    break;
                }
                Err(error) => {
                    tracing::warn!(
                        "[conn-{}] replacement registration failed: {}; old generation remains active",
                        conn_index,
                        error,
                    );
                    let _ = replacement.join.await;
                    if current
                        .as_ref()
                        .is_none_or(|generation| generation.join.is_finished())
                    {
                        if let Some(generation) = current.take() {
                            let _ = generation.join.await;
                        }
                        current_reload_token = None;
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(replacement_backoff)).await;
                    replacement_backoff = (replacement_backoff * 2).min(MAX_BACKOFF_SECS);
                }
            }
        }
    }
}

pub(crate) async fn current_tunnel_config(app_config: &AppConfig) -> (String, Vec<TunnelEntry>) {
    let cfg = app_config.reloadable.lock().await;
    (cfg.tunnel_client_id.clone(), cfg.tunnel_entries.clone())
}

pub(crate) fn retirement_delay(max_age_secs: u64) -> impl std::future::Future<Output = ()> {
    let duration = if max_age_secs == 0 {
        None
    } else {
        crate::tunnel::client::max_age_with_jitter(
            Some(Duration::from_secs(max_age_secs)),
            next_tunnel_conn_seed() as usize,
        )
    };
    async move {
        match duration {
            Some(duration) => tokio::time::sleep(duration).await,
            None => std::future::pending().await,
        }
    }
}

pub(crate) fn join_result_message(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> String {
    match result {
        Ok(Ok(())) => "closed".to_string(),
        Ok(Err(error)) => error.to_string(),
        Err(error) => format!("task failed: {error}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_tls_generation(
    url: Url,
    cert_path: std::path::PathBuf,
    host: String,
    stream_window: u32,
    client_id: String,
    entries: Vec<TunnelEntry>,
    idle_timeout_secs: usize,
    conn_index: usize,
) -> TlsGeneration {
    let (ready_tx, ready_rx) = oneshot::channel();
    let (drain_tx, drain_rx) = oneshot::channel();
    let join = tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);
        let result = run_tunnel_connection_tls(
            &url,
            &cert_path,
            &host,
            stream_window,
            &client_id,
            &entries,
            idle_timeout_secs,
            conn_index,
            &mut ready_tx,
            drain_rx,
        )
        .await;
        if let Some(ready_tx) = ready_tx {
            let message = result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_else(|| "generation exited before registration".to_string());
            let _ = ready_tx.send(Err(message));
        }
        result
    });
    TlsGeneration {
        ready: Some(ready_rx),
        drain: Some(drain_tx),
        join,
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
    conn_index: usize,
    ready_tx: &mut Option<oneshot::Sender<std::result::Result<(), String>>>,
    mut drain_rx: oneshot::Receiver<()>,
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
    if let Some(ready_tx) = ready_tx.take() {
        let _ = ready_tx.send(Ok(()));
    }

    tracing::info!(
        "[conn-{}] Tunnel client ready, waiting for reverse streams...",
        conn_index
    );
    let semaphore = Arc::new(Semaphore::new(MAX_TUNNEL_REVERSE_STREAMS));
    let mut reverse_tasks = JoinSet::new();

    // Reject-path helper: when the reverse-stream semaphore is full, send an
    // explicit FIN before dropping the stream. A bare drop only fires
    // `StreamClose` via `MuxStream::Drop`'s `try_send`, which is lost when the
    // control channel is saturated (exactly the overload condition that made
    // the semaphore fill up) — the server-side visitor would then hang until
    // its own read timeout. A direct FLAG_FIN frame bypasses the control
    // channel and reaches the visitor even under load.
    async fn reject_reverse_stream<
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    >(
        stream_send: &mut W,
        stream_recv: &mut R,
        conn_index: usize,
    ) {
        metrics::counter!("tunnel_reverse_streams_rejected").increment(1);
        tracing::warn!(
            "[conn-{}] max tunnel reverse streams ({}) reached, rejecting stream",
            conn_index,
            MAX_TUNNEL_REVERSE_STREAMS
        );
        let ev = event::new_fin_event(0);
        let _ = event::write_event(stream_send, ev).await;
        // Explicit shutdown so the visitor sees EOF even if the FIN above is
        // superseded by transport teardown.
        let _ = tokio::io::AsyncWriteExt::shutdown(stream_send).await;
        let _ = stream_recv;
    }

    // Liveness ticker for the control connection. When reverse streams are
    // active their data flow proves the link is up and `ping()` returns
    // without touching the wire; when the connection is idle the ping forces
    // a round-trip so a half-open link (NAT session silently dropped) is
    // detected within one interval instead of hanging `accept_stream` until
    // TCP keepalive eventually trips (default: hours).
    let mut ping_tick = tokio::time::interval(TUNNEL_PING_INTERVAL);
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = &mut drain_rx => {
                tracing::info!("[conn-{}] draining TLS tunnel generation", conn_index);
                event::write_event(&mut send, event::new_drain_event(0)).await?;
                tokio::io::AsyncWriteExt::flush(&mut send).await?;
                break;
            }
            _ = ping_tick.tick() => {
                if let Err(e) = conn.ping().await {
                    tracing::error!("[conn-{}] tunnel control ping failed: {}", conn_index, e);
                    return Err(anyhow!("tunnel control connection lost: {}", e));
                }
            }
            Some(result) = reverse_tasks.join_next(), if !reverse_tasks.is_empty() => {
                if let Err(error) = result {
                    tracing::warn!("[conn-{}] reverse stream task failed: {}", conn_index, error);
                }
            }
            result = conn.accept_stream() => {
                let (mut stream_send, mut stream_recv) = match result {
                    Ok(streams) => streams,
                    Err(e) => {
                        tracing::error!("[conn-{}] accept_stream failed: {}", conn_index, e);
                        return Err(e);
                    }
                };
                match semaphore.clone().try_acquire_owned() {
                    Ok(permit) => {
                        reverse_tasks.spawn(async move {
                            let _permit = permit;
                            if let Err(e) = handle_reverse_stream(
                                &mut stream_recv,
                                &mut stream_send,
                                idle_timeout_secs,
                            ).await {
                                tracing::warn!("Reverse stream error: {}", e);
                            }
                        });
                    }
                    Err(_) => {
                        reverse_tasks.spawn(async move {
                            reject_reverse_stream(
                                &mut stream_send,
                                &mut stream_recv,
                                conn_index,
                            ).await;
                        });
                    }
                }
            }
        }
    }

    while let Some(result) = reverse_tasks.join_next().await {
        if let Err(error) = result {
            tracing::warn!(
                "[conn-{}] reverse stream task failed: {}",
                conn_index,
                error
            );
        }
    }
    drop(recv);
    drop(send);
    Ok(())
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
