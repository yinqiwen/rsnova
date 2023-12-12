use anyhow::anyhow;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

use url::Url;

use super::client::mux_client_loop;
use super::client::MuxClient;
use super::client::MuxConnection;
use super::Message;

pub struct S2NQuicConnection {
    // inner: Option<quinn::Connection>,
    // endpoint: Arc<quinn::Endpoint>,
    inner: Option<s2n_quic::Connection>,
    endpoint: Arc<s2n_quic::client::Client>,
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
}

impl MuxClient<S2NQuicConnection> {
    pub async fn from(
        url: &Url,
        cert_path: &Path,
        host: &String,
        count: usize,
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
                    client.conns.push(quic_conn);
                }
                tokio::spawn(mux_client_loop(client, receiver));
                Ok(sender)
            }
            _ => Err(anyhow!("unsupported schema:{:?}", url.scheme())),
        }
    }
}

fn new_s2n_quic_endpoint(_url: &Url, cert_path: &Path) -> anyhow::Result<s2n_quic::client::Client> {
    let client = s2n_quic::client::Client::builder()
        .with_tls(cert_path)?
        .with_io("0.0.0.0:0")?
        .start()?;
    Ok(client)
}

async fn new_s2n_quic_connection(
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
    // ensure the connection doesn't time out with inactivity
    connection.keep_alive(true)?;
    Ok(connection)
}

pub async fn new_quic_client(
    url: &Url,
    cert_path: &Path,
    host: &String,
    count: usize,
) -> anyhow::Result<mpsc::UnboundedSender<Message>> {
    MuxClient::<S2NQuicConnection>::from(url, cert_path, host, count).await
}
