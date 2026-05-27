use anyhow::anyhow;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;
use url::Url;

// use rustls::crypto::{aws_lc_rs as provider, CryptoProvider};

use super::client::mux_client_loop;
use super::client::MuxClient;
use super::client::MuxConnection;
use super::client::{ProxySender, PROXY_CHANNEL_CAPACITY};
use super::Message;
use crate::mux::event;
use crate::mux::MuxStream;
use crate::mux::{self};
use crate::tunnel::ALPN_QUIC_HTTP;
use crate::utils::read_tokio_tls_certs;

pub struct TlsConnection {
    pub(crate) inner: Option<mux::Connection>,
    pub(crate) id: u32,
    pub(crate) stream_window: u32,
}

impl TlsConnection {
    pub fn new(stream_window: u32) -> Self {
        Self {
            inner: None,
            id: 0,
            stream_window,
        }
    }
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
            Some(c) => match c.ping().await {
                Ok(()) => Ok(()),
                Err(e) => {
                    // Tear down the old mux task before dropping it. Otherwise
                    // it keeps running on the half-open TLS link until TCP
                    // keepalive eventually trips — exactly the failure mode
                    // ping was added to detect.
                    c.close();
                    self.inner = None;
                    tracing::error!("ping failed: {}", e);
                    Err(e)
                }
            },
        }
    }
    async fn connect(&mut self, url: &Url, key_path: &Path, host: &str) -> anyhow::Result<()> {
        match new_tls_connection(url, key_path, host).await {
            Ok(c) => {
                let (r, w) = tokio::io::split(c);
                let mux_conn = mux::Connection::new_with_stream_window(
                    r,
                    w,
                    mux::Mode::Client,
                    self.id,
                    self.stream_window,
                );
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
                    c.close();
                    self.inner = None;
                    tracing::error!("failed to open stream: {}", e);
                    Err(e)
                }
            },
        }
    }

    async fn accept_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)> {
        match &mut self.inner {
            None => Err(anyhow!("null connection")),
            Some(c) => match c.accept_stream().await {
                Ok(stream) => {
                    let (r, w) = tokio::io::split(stream);
                    Ok((w, r))
                }
                Err(e) => {
                    c.close();
                    self.inner = None;
                    Err(e)
                }
            },
        }
    }

    fn set_connection(&mut self, new_c: Self) {
        *self = new_c;
    }
}

impl MuxClient<TlsConnection> {
    pub async fn from(
        url: &Url,
        cert_path: &Path,
        host: &String,
        count: usize,
        idle_timeout_secs: usize,
        stream_window: u32,
    ) -> anyhow::Result<ProxySender> {
        match url.scheme() {
            "tls" => {
                let (sender, receiver) = mpsc::channel::<Message>(PROXY_CHANNEL_CAPACITY);
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
                        stream_window,
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
                    if let Some(ref mut conn) = tls_conn.inner {
                        let auth_stream = conn.open_stream().await?;
                        let (mut auth_r, mut auth_w) = tokio::io::split(auth_stream);
                        let auth_req = event::AuthRequest::Proxy;
                        let ev = event::new_auth_event(0, &auth_req)?;
                        event::write_event(&mut auth_w, ev).await?;

                        let ack_ev = event::read_event(&mut auth_r).await?;
                        if ack_ev.header.flags() != event::FLAG_AUTH_ACK {
                            return Err(anyhow!(
                                "proxy auth failed: unexpected flag {}",
                                ack_ev.header.flags()
                            ));
                        }
                        tracing::info!("TLS connection:{} auth completed (proxy mode)", i);
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
    let host = url.host_str().ok_or_else(|| anyhow!("url has no host"))?;
    let remote = (host, url.port().unwrap_or(443))
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("couldn't resolve to an address"))?;

    let certs = read_tokio_tls_certs(cert_path)?;

    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        if let Err(e) = roots.add(cert) {
            tracing::warn!("add cert to root store failed: {}", e);
        }
    }

    let mut client_crypto = tokio_rustls::rustls::ClientConfig::builder()
        // .with_safe_defaults()
        .with_root_certificates(roots)
        .with_no_client_auth();

    client_crypto.alpn_protocols = ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();
    client_crypto.enable_early_data = true;

    let connector = TlsConnector::from(Arc::new(client_crypto));
    let stream = TcpStream::connect(&remote).await?;

    let domain = pki_types::ServerName::try_from(domain)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid dnsname"))?
        .to_owned();
    // let domain: pki_types::ServerName<'_> = rustls::ServerName::try_from(domain)
    //     .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid dnsname"))?
    //     .to_owned();

    let stream: tokio_rustls::client::TlsStream<tokio::net::TcpStream> =
        connector.connect(domain, stream).await?;
    // let ciphersuite = stream.get_ref().1.negotiated_cipher_suite().unwrap();
    // tracing::info!("Current ciphersuite: {:?}", ciphersuite.suite());
    Ok(stream)
}

pub async fn new_tls_client(
    url: &Url,
    cert_path: &Path,
    host: &String,
    count: usize,
    idle_timeout_secs: usize,
    stream_window: u32,
) -> anyhow::Result<ProxySender> {
    MuxClient::<TlsConnection>::from(
        url,
        cert_path,
        host,
        count,
        idle_timeout_secs,
        stream_window,
    )
    .await
}
