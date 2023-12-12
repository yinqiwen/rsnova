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
use crate::tunnel::ALPN_QUIC_HTTP;
use crate::utils::read_tokio_tls_certs;

pub struct QuinnConnection {
    inner: Option<quinn::Connection>,
    endpoint: Arc<quinn::Endpoint>,
}

impl MuxConnection for QuinnConnection {
    type SendStream = quinn::SendStream;
    type RecvStream = quinn::RecvStream;
    fn is_valid(&self) -> bool {
        !self.inner.is_none()
    }
    async fn ping(&mut self) -> anyhow::Result<()> {
        match &mut self.inner {
            None => Err(anyhow!("null connection")),
            Some(_) => {
                let _ = self.open_stream().await?;
                Ok(())
            }
        }
    }
    async fn connect(&mut self, url: &Url, _key_path: &Path, host: &str) -> anyhow::Result<()> {
        match &mut self.inner {
            None => match new_quic_connection(&self.endpoint, url, host).await {
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
            Some(c) => match c.open_bi().await {
                Err(e) => {
                    let _ = c.close(
                        quinn::VarInt::from_u32(48100),
                        "open stream failed".as_bytes(),
                    );
                    self.inner = None;
                    Err(e.into())
                }
                Ok((send, recv)) => Ok((send, recv)),
            },
        }
    }
}
impl MuxClient<QuinnConnection> {
    pub async fn from(
        url: &Url,
        cert_path: &Path,
        host: &str,
        count: usize,
    ) -> anyhow::Result<mpsc::UnboundedSender<Message>> {
        match url.scheme() {
            "quic" => {
                let (sender, receiver) = mpsc::unbounded_channel::<Message>();
                let mut client: MuxClient<QuinnConnection> = MuxClient {
                    url: url.clone(),
                    conns: Vec::new(),
                    host: String::from(host),
                    cursor: 0,
                    cert: Some(PathBuf::from(cert_path)),
                };
                let endpoint = new_quic_endpoint(url, cert_path)?;
                let endpoint = Arc::new(endpoint);
                for i in 0..count {
                    let mut quic_conn = QuinnConnection {
                        endpoint: endpoint.clone(),
                        inner: None,
                    };
                    match quic_conn.connect(url, cert_path, &host).await {
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
                return Ok(sender);
            }
            _ => {
                return Err(anyhow!("unsupported schema:{:?}", url.scheme()));
            }
        }
    }
}

fn new_quic_endpoint(_url: &Url, cert_path: &Path) -> anyhow::Result<quinn::Endpoint> {
    let certs = read_tokio_tls_certs(&cert_path)?;

    let certs = read_tokio_tls_certs(cert_path)?;
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        roots.add(cert).unwrap();
    }

    let mut client_crypto = rustls::ClientConfig::builder()
        // .with_safe_defaults()
        .with_root_certificates(roots)
        .with_no_client_auth();

    client_crypto.alpn_protocols = ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();

    let client_config = quinn::ClientConfig::new(Arc::new(client_crypto));
    let mut endpoint = quinn::Endpoint::client("[::]:0".parse().unwrap())?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}
async fn new_quic_connection(
    endpoint: &quinn::Endpoint,
    url: &Url,
    host: &str,
) -> anyhow::Result<quinn::Connection> {
    let remote = (url.host_str().unwrap(), url.port().unwrap_or(4433))
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("couldn't resolve to an address"))?;

    let conn: quinn::Connection = endpoint
        .connect(remote, host)?
        .await
        .map_err(|e: quinn::ConnectionError| anyhow!("failed to connect: {}", e))?;
    Ok(conn)
}

pub async fn new_quic_client(
    url: &Url,
    cert_path: &Path,
    host: &str,
    count: usize,
) -> anyhow::Result<mpsc::UnboundedSender<Message>> {
    MuxClient::<QuinnConnection>::from(url, cert_path, host, count).await
}
