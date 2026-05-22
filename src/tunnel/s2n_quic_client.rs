use anyhow::anyhow;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use url::Url;

use super::client::mux_client_loop;
use super::client::MuxClient;
use super::client::MuxConnection;
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
}

impl MuxClient<S2NQuicConnection> {
    pub async fn from(
        url: &Url,
        cert_path: &Path,
        host: &String,
        count: usize,
        idle_timeout_secs: usize,
    ) -> anyhow::Result<mpsc::UnboundedSender<Message>> {
        match url.scheme() {
            "quic" => {
                let (sender, receiver) = mpsc::unbounded_channel::<Message>();
                let mut client: MuxClient<S2NQuicConnection> = MuxClient {
                    url: url.clone(),
                    conns: Vec::new(),
                    host: String::from(host),
                    cursor: 0,
                    cert: Some(PathBuf::from(cert_path)),
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
                    client.conns.push(quic_conn);
                }
                tokio::spawn(mux_client_loop(client, receiver, idle_timeout_secs));
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

/// Tunnel client loop for QUIC mode using Connection::split()
pub async fn start_tunnel_client_quic(
    url: &Url,
    cert_path: &Path,
    host: &str,
    client_id: &str,
    entries: Vec<TunnelEntry>,
    idle_timeout_secs: usize,
) -> anyhow::Result<()> {
    const INITIAL_BACKOFF_SECS: u64 = 1;
    const MAX_BACKOFF_SECS: u64 = 60;

    let mut backoff_secs = INITIAL_BACKOFF_SECS;
    loop {
        let start = std::time::Instant::now();
        let result = run_quic_tunnel_connection(
            url,
            cert_path,
            host,
            client_id,
            &entries,
            idle_timeout_secs,
        )
        .await;

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

    while let Ok(Some(stream)) = acceptor.accept_bidirectional_stream().await {
        let (mut recv_stream, mut send_stream) = stream.split();
        tokio::spawn(async move {
            if let Err(e) = crate::tunnel::tunnel_client::handle_reverse_stream(
                &mut recv_stream,
                &mut send_stream,
                idle_timeout_secs,
            )
            .await
            {
                tracing::warn!("QUIC reverse stream error: {}", e);
            }
        });
    }
    Ok(())
}

pub async fn new_quic_client(
    url: &Url,
    cert_path: &Path,
    host: &String,
    count: usize,
    idle_timeout_secs: usize,
) -> anyhow::Result<mpsc::UnboundedSender<Message>> {
    MuxClient::<S2NQuicConnection>::from(url, cert_path, host, count, idle_timeout_secs).await
}
