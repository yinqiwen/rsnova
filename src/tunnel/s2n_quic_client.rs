use anyhow::anyhow;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use url::Url;

use super::client::mux_client_loop;
use super::client::MuxClient;
use super::client::MuxConnection;
use super::client::{ConnParams, ProxySender, PROXY_CHANNEL_CAPACITY};
use super::Message;
use crate::mux::event::{
    self, AuthAck, AuthRequest, RegisterRequest, TunnelEntry, FLAG_AUTH_ACK,
};

pub struct S2NQuicConnection {
    pub(crate) inner: Option<s2n_quic::Connection>,
    pub(crate) endpoint: Arc<s2n_quic::client::Client>,
}

impl MuxConnection for S2NQuicConnection {
    type SendStream = s2n_quic::stream::SendStream;
    type RecvStream = s2n_quic::stream::ReceiveStream;
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
                    Ok((s, r))
                }
            },
        }
    }

    async fn accept_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)> {
        Err(anyhow!("QUIC accept_stream not yet implemented for tunnel mode"))
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
        // QUIC doesn't use mux::Connection, no stream counter available
        0
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
    pub async fn from(
        url: &Url,
        cert_path: &Path,
        host: &String,
        count: usize,
        idle_timeout_secs: usize,
        max_age_secs: u64,
        ping_interval_secs: u64,
        ping_fail_threshold: u32,
    ) -> anyhow::Result<ProxySender> {
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
                    };
                    match quic_conn.connect(url, cert_path, host).await {
                        Err(e) => {
                            if i == 0 {
                                return Err(e);
                            }
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
                    host: host.clone(),
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
    loop {
        let (client_id, entries) = {
            let cfg = app_config.reloadable.lock().await;
            (cfg.tunnel_client_id.clone(), cfg.tunnel_entries.clone())
        };

        let start = std::time::Instant::now();
        let token = app_config.reload_token_clone().await;

        let result = tokio::select! {
            r = run_quic_tunnel_connection(
                url,
                cert_path,
                host,
                &client_id,
                &entries,
                idle_timeout_secs,
                max_age_secs,
                conn_index,
            ) => r,
            _ = token.cancelled() => {
                tracing::info!("[conn-{}] Config reloaded, reconnecting QUIC tunnel with new entries...", conn_index);
                backoff_secs = INITIAL_BACKOFF_SECS;
                continue;
            }
        };

        // Reset backoff if connection was productive (lasted > 30s)
        if start.elapsed() > Duration::from_secs(30) {
            backoff_secs = INITIAL_BACKOFF_SECS;
        }

        tracing::info!(
            "[conn-{}] QUIC tunnel connection lost ({}), reconnecting in {}s...",
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

async fn run_quic_tunnel_connection(
    url: &Url,
    cert_path: &Path,
    host: &str,
    client_id: &str,
    entries: &[TunnelEntry],
    idle_timeout_secs: usize,
    max_age_secs: u64,
    conn_index: usize,
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
    drop(send);
    drop(recv);

    let (_handle, mut acceptor) = connection.split();

    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    if max_age_secs > 0 {
        let seed = crate::tunnel::tunnel_client::next_tunnel_conn_seed() as usize;
        let jitter = super::client::retirement_jitter_secs(seed);
        let retire_at = Instant::now() + Duration::from_secs((max_age_secs as i64 + jitter).max(0) as u64);

        loop {
            handles.retain(|h| !h.is_finished());

            tokio::select! {
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(retire_at)) => {
                    tracing::info!("[conn-{}] QUIC tunnel connection reached max age, draining...", conn_index);
                    futures::future::join_all(handles).await;
                    return Ok(());
                }
                result = acceptor.accept_bidirectional_stream() => {
                    match result {
                        Ok(Some(stream)) => {
                            let (mut recv_stream, mut send_stream) = stream.split();
                            handles.push(tokio::spawn(async move {
                                if let Err(e) = crate::tunnel::tunnel_client::handle_reverse_stream(
                                    &mut recv_stream,
                                    &mut send_stream,
                                    idle_timeout_secs,
                                )
                                .await
                                {
                                    tracing::warn!("QUIC reverse stream error: {}", e);
                                }
                            }));
                        }
                        Ok(None) => {
                            futures::future::join_all(handles).await;
                            return Ok(());
                        }
                        Err(e) => {
                            futures::future::join_all(handles).await;
                            return Err(anyhow!("[conn-{}] QUIC accept error: {}", conn_index, e));
                        }
                    }
                }
            }
        }
    } else {
        // No retirement — accept streams forever
        loop {
            handles.retain(|h| !h.is_finished());

            match acceptor.accept_bidirectional_stream().await {
                Ok(Some(stream)) => {
                    let (mut recv_stream, mut send_stream) = stream.split();
                    handles.push(tokio::spawn(async move {
                        if let Err(e) = crate::tunnel::tunnel_client::handle_reverse_stream(
                            &mut recv_stream,
                            &mut send_stream,
                            idle_timeout_secs,
                        )
                        .await
                        {
                            tracing::warn!("QUIC reverse stream error: {}", e);
                        }
                    }));
                }
                Ok(None) => {
                    futures::future::join_all(handles).await;
                    return Ok(());
                }
                Err(e) => {
                    futures::future::join_all(handles).await;
                    return Err(anyhow!("[conn-{}] QUIC accept error: {}", conn_index, e));
                }
            }
        }
    }
}

pub async fn new_quic_client(
    url: &Url,
    cert_path: &Path,
    host: &String,
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
