use anyhow::anyhow;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;
use url::Url;

use super::client::mux_client_loop;
use super::client::MuxClient;
use super::client::MuxConnection;
use super::Message;
use crate::mux::MuxStream;
use crate::mux::{self};
use crate::tunnel::ALPN_QUIC_HTTP;
use crate::utils::read_tokio_tls_certs;

pub struct TlsConnection {
    inner: Option<mux::Connection>,
    id: u32,
}

impl MuxConnection for TlsConnection {
    type SendStream = tokio::io::WriteHalf<MuxStream>;
    type RecvStream = tokio::io::ReadHalf<MuxStream>;
    fn is_valid(&self) -> bool {
        self.inner.is_some()
    }

    async fn ping(&mut self) -> anyhow::Result<()> {
        match &mut self.inner {
            None => Err(anyhow!("null connection")),
            Some(c) => c.ping().await,
        }
    }
    async fn connect(&mut self, url: &Url, key_path: &Path, host: &str) -> anyhow::Result<()> {
        match new_tls_connection(url, key_path, host).await {
            Ok(c) => {
                let (r, w) = tokio::io::split(c);
                let mux_conn = mux::Connection::new(r, w, mux::Mode::Client, self.id);
                self.inner = Some(mux_conn);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)> {
        match &mut self.inner {
            None => Err(anyhow!("null connection")),
            Some(c) => match c.open_stream().await {
                Ok(stream) => {
                    let (r, w) = tokio::io::split(stream);
                    Ok((w, r))
                }
                Err(e) => {
                    self.inner = None;
                    tracing::error!("failed to open stream: {}", e);
                    Err(e)
                }
            },
        }
    }
}

impl MuxClient<TlsConnection> {
    pub async fn from(
        url: &Url,
        cert_path: &Path,
        host: &String,
        count: usize,
        idle_timeout_secs: usize,
    ) -> anyhow::Result<mpsc::UnboundedSender<Message>> {
        match url.scheme() {
            "tls" => {
                let (sender, receiver) = mpsc::unbounded_channel::<Message>();
                let mut client: MuxClient<TlsConnection> = MuxClient {
                    url: url.clone(),
                    conns: Vec::new(),
                    host: String::from(host),
                    cursor: 0,
                    cert: Some(PathBuf::from(cert_path)),
                };
                for i in 0..count {
                    let mut tls_conn: TlsConnection = TlsConnection {
                        inner: None,
                        id: i as u32,
                    };
                    match tls_conn.connect(url, cert_path, host).await {
                        Err(e) => {
                            if i == 0 {
                                return Err(e);
                            }
                        }
                        _ => {
                            tracing::info!("TLS connection:{} established!", i);
                        }
                    }
                    client.conns.push(tls_conn);
                }
                tokio::spawn(mux_client_loop(client, receiver, idle_timeout_secs));
                Ok(sender)
            }
            _ => Err(anyhow!("unsupported schema:{:?}", url.scheme())),
        }
    }
}

async fn new_tls_connection(
    url: &Url,
    cert_path: &Path,
    domain: &str,
) -> anyhow::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let remote = (url.host_str().unwrap(), url.port().unwrap_or(4433))
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("couldn't resolve to an address"))?;

    let certs = read_tokio_tls_certs(cert_path)?;

    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        roots.add(&cert).unwrap();
    }

    let mut client_crypto = tokio_rustls::rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(roots)
        .with_no_client_auth();

    client_crypto.alpn_protocols = ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();

    let connector = TlsConnector::from(Arc::new(client_crypto));
    let stream = TcpStream::connect(&remote).await?;

    // let domain = pki_types::ServerName::try_from(domain)
    //     .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid dnsname"))?
    //     .to_owned();
    let domain = rustls::ServerName::try_from(domain)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid dnsname"))?
        .to_owned();

    let stream: tokio_rustls::client::TlsStream<tokio::net::TcpStream> =
        connector.connect(domain, stream).await?;
    Ok(stream)
}

pub async fn new_tls_client(
    url: &Url,
    cert_path: &Path,
    host: &String,
    count: usize,
    idle_timeout_secs: usize,
) -> anyhow::Result<mpsc::UnboundedSender<Message>> {
    MuxClient::<TlsConnection>::from(url, cert_path, host, count, idle_timeout_secs).await
}
