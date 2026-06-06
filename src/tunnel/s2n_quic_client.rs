use anyhow::anyhow;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use url::Url;

use super::client::mux_client_loop;
use super::client::MuxClient;
use super::client::MuxConnection;
use super::client::PoolConnection;
use super::client::{ProxySender, PROXY_CHANNEL_CAPACITY};
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
}

fn spawn_quic_replacement(
    url: Url,
    cert_path: PathBuf,
    host: String,
    endpoint: Arc<s2n_quic::client::Client>,
    sender: ProxySender,
) {
    tokio::spawn(async move {
        let max_retries = 5u32;
        let mut backoff = Duration::from_secs(1);
        for attempt in 0..max_retries {
            tracing::info!("QUIC replacement connection attempt {}", attempt + 1);
            let mut quic_conn = S2NQuicConnection {
                endpoint: endpoint.clone(),
                inner: None,
            };
            if let Err(e) = quic_conn.connect(&url, &cert_path, &host).await {
                tracing::warn!("QUIC replacement connect failed (attempt {}): {}", attempt + 1, e);
                if attempt + 1 < max_retries {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
                continue;
            }
            let success = 'auth: {
                let Some(ref mut connection) = quic_conn.inner else { break 'auth false; };
                let auth_stream = match connection.open_bidirectional_stream().await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("QUIC replacement: open auth stream failed: {}", e);
                        break 'auth false;
                    }
                };
                let (mut auth_r, mut auth_w) = auth_stream.split();
                let auth_req = event::AuthRequest::Proxy;
                let ev = match event::new_auth_event(0, &auth_req) {
                    Ok(ev) => ev,
                    Err(e) => {
                        tracing::warn!("QUIC replacement: create auth event failed: {}", e);
                        break 'auth false;
                    }
                };
                if event::write_event(&mut auth_w, ev).await.is_err() {
                    tracing::warn!("QUIC replacement: auth write failed");
                    break 'auth false;
                }
                match event::read_event(&mut auth_r).await {
                    Ok(ack_ev) if ack_ev.header.flags() == event::FLAG_AUTH_ACK => true,
                    _ => {
                        tracing::warn!("QUIC replacement: unexpected auth response");
                        false
                    }
                }
            };
            if success {
                tracing::info!("QUIC replacement connection authenticated");
                let _ = sender.send(Message::ReplaceConnection(Box::new(quic_conn))).await;
                return;
            }
            quic_conn.close();
            if attempt + 1 < max_retries {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
        tracing::error!("QUIC replacement connection failed after {} attempts", max_retries);
    });
}

impl MuxClient<S2NQuicConnection> {
    pub async fn from(
        url: &Url,
        cert_path: &Path,
        host: &String,
        count: usize,
        idle_timeout_secs: usize,
        max_age_secs: u64,
    ) -> anyhow::Result<ProxySender> {
        match url.scheme() {
            "quic" => {
                let (sender, receiver) = mpsc::channel::<Message>(PROXY_CHANNEL_CAPACITY);
                let (retirement_tx, mut retirement_rx) = mpsc::unbounded_channel::<usize>();
                let mut client: MuxClient<S2NQuicConnection> = MuxClient {
                    url: url.clone(),
                    conns: Vec::new(),
                    host: String::from(host),
                    cursor: 0,
                    cert: Some(PathBuf::from(cert_path)),
                    max_age_secs,
                    retirement_notify: retirement_tx,
                };
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
                    client.conns.push(PoolConnection::new(quic_conn, max_age_secs, i));
                }
                tokio::spawn(mux_client_loop(client, receiver, idle_timeout_secs));
                let replacement_sender = sender.clone();
                let replacement_url = url.clone();
                let replacement_cert = cert_path.to_path_buf();
                let replacement_host = host.clone();
                let replacement_endpoint = endpoint.clone();
                tokio::spawn(async move {
                    while let Some(_idx) = retirement_rx.recv().await {
                        spawn_quic_replacement(
                            replacement_url.clone(),
                            replacement_cert.clone(),
                            replacement_host.clone(),
                            replacement_endpoint.clone(),
                            replacement_sender.clone(),
                        );
                    }
                });
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
    let remote: std::net::SocketAddr = (url.host_str().unwrap(), url.port().unwrap_or(4433))
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("couldn't resolve to an address"))?;

    let connect = s2n_quic::client::Connect::new(remote).with_server_name(host);
    let mut connection = endpoint.connect(connect).await?;
    tracing::info!("conncect s2n quic success");
    connection.keep_alive(true)?;
    Ok(connection)
}

/// Tunnel client loop for QUIC mode with hot-reload support
pub async fn start_tunnel_client_quic(
    url: &Url,
    cert_path: &Path,
    host: &str,
    app_config: Arc<crate::app_config::AppConfig>,
    idle_timeout_secs: usize,
    max_age_secs: u64,
) -> anyhow::Result<()> {
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
            r = run_quic_tunnel_connection(
                url,
                cert_path,
                host,
                &client_id,
                &entries,
                idle_timeout_secs,
                max_age_secs,
            ) => r,
            _ = token.cancelled() => {
                tracing::info!("Config reloaded, reconnecting QUIC tunnel with new entries...");
                backoff_secs = INITIAL_BACKOFF_SECS;
                continue;
            }
        };

        // Reset backoff if connection was productive (lasted > 30s)
        if start.elapsed() > Duration::from_secs(30) {
            backoff_secs = INITIAL_BACKOFF_SECS;
        }

        tracing::info!(
            "QUIC tunnel connection lost ({}), reconnecting in {}s...",
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

    let seed = crate::tunnel::tunnel_client::next_tunnel_conn_seed() as usize;
    let jitter = super::client::retirement_jitter_secs(seed);
    let retire_at = Instant::now() + Duration::from_secs((max_age_secs as i64 + jitter).max(0) as u64);
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    loop {
        handles.retain(|h| !h.is_finished());

        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(retire_at)) => {
                tracing::info!("QUIC tunnel connection reached max age, draining...");
                for h in handles {
                    let _ = h.await;
                }
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
                        for h in handles {
                            let _ = h.await;
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        for h in handles {
                            let _ = h.await;
                        }
                        return Err(anyhow!("QUIC accept error: {}", e));
                    }
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
) -> anyhow::Result<ProxySender> {
    MuxClient::<S2NQuicConnection>::from(url, cert_path, host, count, idle_timeout_secs, max_age_secs).await
}
