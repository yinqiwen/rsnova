use anyhow::anyhow;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use url::Url;

use super::Message;
use super::client::MuxClient;
use super::client::MuxConnection;
use super::client::mux_client_loop;
use super::client::{ConnParams, PROXY_CHANNEL_CAPACITY, ProxySender, validate_pool_config};
use crate::mux::event::{self, AuthAck, AuthRequest, FLAG_AUTH_ACK, RegisterRequest, TunnelEntry};

pub struct S2NQuicConnection {
    pub(crate) inner: Option<s2n_quic::Connection>,
    pub(crate) endpoint: Arc<s2n_quic::client::Client>,
    active_streams: Arc<AtomicUsize>,
}

struct QuicStreamLease {
    active_streams: Arc<AtomicUsize>,
}

struct QuicTunnelGeneration {
    ready: Option<oneshot::Receiver<std::result::Result<(), String>>>,
    drain: Option<oneshot::Sender<()>>,
    join: JoinHandle<anyhow::Result<()>>,
}

impl QuicTunnelGeneration {
    async fn wait_ready(&mut self) -> anyhow::Result<()> {
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

impl QuicStreamLease {
    fn acquire(active_streams: Arc<AtomicUsize>) -> Arc<Self> {
        active_streams.fetch_add(1, Ordering::AcqRel);
        Arc::new(Self { active_streams })
    }
}

impl Drop for QuicStreamLease {
    fn drop(&mut self) {
        self.active_streams.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct TrackedQuicSendStream {
    inner: s2n_quic::stream::SendStream,
    _lease: Arc<QuicStreamLease>,
}

impl AsyncWrite for TrackedQuicSendStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub struct TrackedQuicReceiveStream {
    inner: s2n_quic::stream::ReceiveStream,
    _lease: Arc<QuicStreamLease>,
}

impl AsyncRead for TrackedQuicReceiveStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl MuxConnection for S2NQuicConnection {
    type SendStream = TrackedQuicSendStream;
    type RecvStream = TrackedQuicReceiveStream;
    fn is_valid(&self) -> bool {
        self.inner.is_some()
    }
    async fn ping(&mut self) -> anyhow::Result<()> {
        match &mut self.inner {
            None => Err(anyhow!("null connection")),
            Some(c) => {
                if let Err(e) = c.ping() {
                    c.close(s2n_quic::application::Error::UNKNOWN);
                    self.inner = None;
                    tracing::info!("ping fail:{}", e);
                    Err(e.into())
                } else {
                    Ok(())
                }
            }
        }
    }
    async fn connect(&mut self, url: &Url, _key_path: &Path, host: &str) -> anyhow::Result<()> {
        match &mut self.inner {
            None => match new_s2n_quic_connection(&self.endpoint, url, host).await {
                Ok(c) => {
                    self.inner = Some(c);
                    Ok(())
                }
                Err(e) => Err(e),
            },
            Some(_) => Err(anyhow!("non null connection")),
        }
    }
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)> {
        match &mut self.inner {
            None => Err(anyhow!("null connection")),
            Some(c) => match c.open_bidirectional_stream().await {
                Err(e) => {
                    c.close(s2n_quic::application::Error::UNKNOWN);
                    self.inner = None;
                    tracing::info!("open stream fail:{}", e);
                    Err(e.into())
                }
                Ok(stream) => {
                    let (r, s) = stream.split();
                    let lease = QuicStreamLease::acquire(self.active_streams.clone());
                    Ok((
                        TrackedQuicSendStream {
                            inner: s,
                            _lease: lease.clone(),
                        },
                        TrackedQuicReceiveStream {
                            inner: r,
                            _lease: lease,
                        },
                    ))
                }
            },
        }
    }

    async fn accept_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)> {
        Err(anyhow!(
            "QUIC accept_stream not yet implemented for tunnel mode"
        ))
    }

    fn set_connection(&mut self, new_c: Self) {
        *self = new_c;
    }

    fn close(&mut self) {
        if let Some(c) = self.inner.take() {
            c.close(s2n_quic::application::Error::UNKNOWN);
        }
    }

    fn active_stream_count(&self) -> usize {
        self.active_streams.load(Ordering::Acquire)
    }

    fn reconnect_with(
        params: &ConnParams,
    ) -> impl std::future::Future<Output = anyhow::Result<Self>> + Send {
        let url = params.url.clone();
        let cert = params.cert_path.clone();
        let host = params.host.clone();
        let endpoint = params.quic_endpoint.clone();
        async move {
            let endpoint = endpoint
                .as_ref()
                .ok_or_else(|| anyhow!("missing quic endpoint in ConnParams"))?;
            let mut c = S2NQuicConnection {
                endpoint: endpoint.clone(),
                inner: None,
                active_streams: Arc::new(AtomicUsize::new(0)),
            };
            c.connect(&url, &cert, &host).await?;
            // Auth handshake
            let connection = c
                .inner
                .as_mut()
                .ok_or_else(|| anyhow!("null quic connection after connect"))?;
            let auth_stream = connection
                .open_bidirectional_stream()
                .await
                .map_err(|e| anyhow!("open auth stream: {}", e))?;
            let (mut auth_r, mut auth_w) = auth_stream.split();
            let auth_req = event::AuthRequest::Proxy;
            let ev = event::new_auth_event(0, &auth_req)?;
            event::write_event(&mut auth_w, ev).await?;
            let ack = event::read_event(&mut auth_r).await?;
            if ack.header.flags() != event::FLAG_AUTH_ACK {
                return Err(anyhow!("reconnect auth failed: unexpected flag"));
            }
            Ok(c)
        }
    }
}

impl MuxClient<S2NQuicConnection> {
    #[allow(clippy::too_many_arguments)]
    pub async fn from(
        url: &Url,
        cert_path: &Path,
        host: &str,
        count: usize,
        idle_timeout_secs: usize,
        max_age_secs: u64,
        ping_interval_secs: u64,
        ping_fail_threshold: u32,
    ) -> anyhow::Result<ProxySender> {
        validate_pool_config(count, ping_interval_secs, ping_fail_threshold)?;
        match url.scheme() {
            "quic" => {
                let (sender, receiver) = mpsc::channel::<Message>(PROXY_CHANNEL_CAPACITY);
                let cancel = tokio_util::sync::CancellationToken::new();
                let pool = Arc::new(MuxClient::<S2NQuicConnection>::new(cancel.clone()));
                let mut initial: Vec<S2NQuicConnection> = Vec::with_capacity(count);
                let endpoint = new_s2n_quic_endpoint(url, cert_path)?;
                let endpoint = Arc::new(endpoint);
                for i in 0..count {
                    let mut quic_conn = S2NQuicConnection {
                        endpoint: endpoint.clone(),
                        inner: None,
                        active_streams: Arc::new(AtomicUsize::new(0)),
                    };
                    match quic_conn.connect(url, cert_path, host).await {
                        Err(e) => {
                            if i == 0 {
                                return Err(e);
                            }
                            tracing::warn!("QUIC connection:{} failed during startup: {}", i, e);
                            continue;
                        }
                        _ => {
                            tracing::info!("QUIC connection:{} established!", i);
                        }
                    }
                    if let Some(ref mut connection) = quic_conn.inner {
                        let auth_stream = connection
                            .open_bidirectional_stream()
                            .await
                            .map_err(|e| anyhow!("open auth stream: {}", e))?;
                        let (mut auth_r, mut auth_w) = auth_stream.split();
                        let auth_req = AuthRequest::Proxy;
                        let ev = event::new_auth_event(0, &auth_req)?;
                        event::write_event(&mut auth_w, ev).await?;

                        let ack_ev = event::read_event(&mut auth_r).await?;
                        if ack_ev.header.flags() != FLAG_AUTH_ACK {
                            return Err(anyhow!(
                                "proxy auth failed: unexpected flag {}",
                                ack_ev.header.flags()
                            ));
                        }
                        tracing::info!("QUIC connection:{} auth completed (proxy mode)", i);
                    }
                    initial.push(quic_conn);
                }
                let params = Arc::new(ConnParams {
                    url: url.clone(),
                    cert_path: cert_path.to_path_buf(),
                    host: host.to_owned(),
                    stream_window: 0, // unused for QUIC
                    max_age: if max_age_secs == 0 {
                        None
                    } else {
                        Some(Duration::from_secs(max_age_secs))
                    },
                    ping_interval: Duration::from_secs(ping_interval_secs),
                    ping_fail_threshold,
                    quic_endpoint: Some(endpoint.clone()),
                });
                tokio::spawn(mux_client_loop(pool.clone(), receiver, idle_timeout_secs));
                tokio::spawn(crate::tunnel::client::pool_monitor(
                    pool.clone(),
                    params,
                    cancel,
                    initial,
                ));
                Ok(sender)
            }
            _ => Err(anyhow!("unsupported schema:{:?}", url.scheme())),
        }
    }
}

pub(crate) fn new_s2n_quic_endpoint(
    _url: &Url,
    cert_path: &Path,
) -> anyhow::Result<s2n_quic::client::Client> {
    let client = s2n_quic::client::Client::builder()
        .with_tls(cert_path)?
        .with_io("0.0.0.0:0")?
        .start()?;
    Ok(client)
}

pub(crate) async fn new_s2n_quic_connection(
    endpoint: &s2n_quic::client::Client,
    url: &Url,
    host: &str,
) -> anyhow::Result<s2n_quic::Connection> {
    let host_str = url.host_str().ok_or_else(|| anyhow!("url has no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("invalid port in URL"))?;
    let remote: std::net::SocketAddr = (host_str, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("couldn't resolve to an address"))?;

    let connect = s2n_quic::client::Connect::new(remote).with_server_name(host);
    let mut connection = endpoint.connect(connect).await?;
    tracing::info!("conncect s2n quic success");
    connection.keep_alive(true)?;
    Ok(connection)
}

/// Tunnel client entry point for QUIC mode with hot-reload support.
/// Spawns `concurrent` independent tunnel client tasks, each with its own
/// connection, reconnection loop, and authentication.
#[allow(clippy::too_many_arguments)]
pub async fn start_tunnel_client_quic(
    url: &Url,
    cert_path: &Path,
    host: &str,
    app_config: Arc<crate::app_config::AppConfig>,
    idle_timeout_secs: usize,
    max_age_secs: u64,
    concurrent: usize,
) -> anyhow::Result<()> {
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
            tunnel_client_loop_quic(
                &url,
                &cert_path,
                &host,
                app_config,
                idle_timeout_secs,
                max_age_secs,
                i,
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
async fn tunnel_client_loop_quic(
    url: &Url,
    cert_path: &Path,
    host: &str,
    app_config: Arc<crate::app_config::AppConfig>,
    idle_timeout_secs: usize,
    max_age_secs: u64,
    conn_index: usize,
) {
    const INITIAL_BACKOFF_SECS: u64 = 1;
    const MAX_BACKOFF_SECS: u64 = 60;

    let mut backoff_secs = INITIAL_BACKOFF_SECS;
    let mut current: Option<QuicTunnelGeneration> = None;
    let mut current_reload_token: Option<tokio_util::sync::CancellationToken> = None;

    loop {
        if current.is_none() {
            let reload_token = app_config.reload_token_clone().await;
            let (client_id, entries) =
                crate::tunnel::tunnel_client::current_tunnel_config(&app_config).await;
            let mut candidate = spawn_quic_tunnel_generation(
                url.clone(),
                cert_path.to_path_buf(),
                host.to_string(),
                client_id,
                entries,
                idle_timeout_secs,
                conn_index,
            );
            match candidate.wait_ready().await {
                Ok(()) => {
                    current = Some(candidate);
                    current_reload_token = Some(reload_token);
                    backoff_secs = INITIAL_BACKOFF_SECS;
                }
                Err(error) => {
                    tracing::warn!(
                        "[conn-{}] QUIC tunnel registration failed: {}; retrying in {}s",
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
        let retirement = crate::tunnel::tunnel_client::retirement_delay(max_age_secs);
        tokio::pin!(retirement);
        let generation = current.as_mut().expect("current generation exists");
        let replace = tokio::select! {
            result = &mut generation.join => {
                tracing::warn!(
                    "[conn-{}] QUIC tunnel generation ended: {}",
                    conn_index,
                    crate::tunnel::tunnel_client::join_result_message(result),
                );
                current = None;
                current_reload_token = None;
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                false
            }
            _ = token.cancelled() => {
                tracing::info!("[conn-{}] QUIC tunnel config changed; starting replacement", conn_index);
                true
            }
            _ = &mut retirement => {
                tracing::info!("[conn-{}] QUIC tunnel reached max age; starting replacement", conn_index);
                true
            }
        };
        if !replace {
            continue;
        }

        let mut replacement_backoff = INITIAL_BACKOFF_SECS;
        loop {
            let replacement_token = app_config.reload_token_clone().await;
            let (client_id, entries) =
                crate::tunnel::tunnel_client::current_tunnel_config(&app_config).await;
            let mut replacement = spawn_quic_tunnel_generation(
                url.clone(),
                cert_path.to_path_buf(),
                host.to_string(),
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
                        "[conn-{}] QUIC replacement registration failed: {}; old generation remains active",
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

#[allow(clippy::too_many_arguments)]
fn spawn_quic_tunnel_generation(
    url: Url,
    cert_path: std::path::PathBuf,
    host: String,
    client_id: String,
    entries: Vec<TunnelEntry>,
    idle_timeout_secs: usize,
    conn_index: usize,
) -> QuicTunnelGeneration {
    let (ready_tx, ready_rx) = oneshot::channel();
    let (drain_tx, drain_rx) = oneshot::channel();
    let join = tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);
        let result = run_quic_tunnel_connection(
            &url,
            &cert_path,
            &host,
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
    QuicTunnelGeneration {
        ready: Some(ready_rx),
        drain: Some(drain_tx),
        join,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_quic_tunnel_connection(
    url: &Url,
    cert_path: &Path,
    host: &str,
    client_id: &str,
    entries: &[TunnelEntry],
    idle_timeout_secs: usize,
    conn_index: usize,
    ready_tx: &mut Option<oneshot::Sender<std::result::Result<(), String>>>,
    mut drain_rx: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let endpoint = new_s2n_quic_endpoint(url, cert_path)?;
    let mut connection = new_s2n_quic_connection(&endpoint, url, host).await?;

    let auth_stream = connection
        .open_bidirectional_stream()
        .await
        .map_err(|e| anyhow!("open auth stream: {}", e))?;
    let (mut recv, mut send) = auth_stream.split();

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
            crate::tunnel::tunnel_client::handle_register_ack(&register_ack)?;
        }
    }
    if let Some(ready_tx) = ready_tx.take() {
        let _ = ready_tx.send(Ok(()));
    }

    let (handle, mut acceptor) = connection.split();
    let semaphore = Arc::new(Semaphore::new(
        crate::tunnel::tunnel_client::MAX_TUNNEL_REVERSE_STREAMS,
    ));
    let mut reverse_tasks = JoinSet::new();

    loop {
        tokio::select! {
            _ = &mut drain_rx => {
                tracing::info!("[conn-{}] draining QUIC tunnel generation", conn_index);
                event::write_event(&mut send, event::new_drain_event(0)).await?;
                tokio::io::AsyncWriteExt::flush(&mut send).await?;
                break;
            }
            Some(result) = reverse_tasks.join_next(), if !reverse_tasks.is_empty() => {
                if let Err(error) = result {
                    tracing::warn!("[conn-{}] QUIC reverse stream task failed: {}", conn_index, error);
                }
            }
            result = acceptor.accept_bidirectional_stream() => {
                let stream = match result {
                    Ok(Some(stream)) => stream,
                    Ok(None) => return Ok(()),
                    Err(error) => {
                        return Err(anyhow!("[conn-{}] QUIC accept error: {}", conn_index, error));
                    }
                };
                match semaphore.clone().try_acquire_owned() {
                    Ok(permit) => {
                        let (mut recv_stream, mut send_stream) = stream.split();
                        reverse_tasks.spawn(async move {
                            let _permit = permit;
                            if let Err(e) = crate::tunnel::tunnel_client::handle_reverse_stream(
                                &mut recv_stream,
                                &mut send_stream,
                                idle_timeout_secs,
                            ).await {
                                tracing::warn!("QUIC reverse stream error: {}", e);
                            }
                        });
                    }
                    Err(_) => {
                        metrics::counter!("tunnel_reverse_streams_rejected").increment(1);
                        tracing::warn!(
                            "[conn-{}] max tunnel reverse streams ({}) reached, rejecting QUIC stream",
                            conn_index,
                            crate::tunnel::tunnel_client::MAX_TUNNEL_REVERSE_STREAMS,
                        );
                        let (recv_stream, mut send_stream) = stream.split();
                        reverse_tasks.spawn(async move {
                            let _ = tokio::io::AsyncWriteExt::shutdown(&mut send_stream).await;
                            drop(recv_stream);
                        });
                    }
                }
            }
        }
    }

    while let Some(result) = reverse_tasks.join_next().await {
        if let Err(error) = result {
            tracing::warn!(
                "[conn-{}] QUIC reverse stream task failed: {}",
                conn_index,
                error
            );
        }
    }
    drop(recv);
    drop(send);
    drop(acceptor);
    drop(handle);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn new_quic_client(
    url: &Url,
    cert_path: &Path,
    host: &str,
    count: usize,
    idle_timeout_secs: usize,
    max_age_secs: u64,
    ping_interval_secs: u64,
    ping_fail_threshold: u32,
) -> anyhow::Result<ProxySender> {
    MuxClient::<S2NQuicConnection>::from(
        url,
        cert_path,
        host,
        count,
        idle_timeout_secs,
        max_age_secs,
        ping_interval_secs,
        ping_fail_threshold,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quic_stream_lease_releases_after_both_halves_drop() {
        let active_streams = Arc::new(AtomicUsize::new(0));
        let lease = QuicStreamLease::acquire(active_streams.clone());
        let send_half = lease.clone();
        let receive_half = lease.clone();
        drop(lease);

        assert_eq!(active_streams.load(Ordering::Acquire), 1);
        drop(send_half);
        assert_eq!(active_streams.load(Ordering::Acquire), 1);
        drop(receive_half);
        assert_eq!(active_streams.load(Ordering::Acquire), 0);
    }
}
