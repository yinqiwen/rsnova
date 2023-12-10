use metrics::{decrement_gauge, increment_gauge};
use std::io;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;
use url::Url;

use anyhow::anyhow;

use crate::mux::event::OpenStreamEvent;
use crate::mux::MuxStream;
use crate::mux::{self, event};
use crate::tunnel::stream::Stream;
use crate::tunnel::ALPN_QUIC_HTTP;

struct OpenStreamRequest {
    tcp_stream: tokio::net::TcpStream,
    event: OpenStreamEvent,
    payload: Option<Vec<u8>>,
}

impl OpenStreamRequest {
    pub fn from(stream: tokio::net::TcpStream, target: String, payload: Option<Vec<u8>>) -> Self {
        Self {
            tcp_stream: stream,
            event: OpenStreamEvent {
                proto: String::from("tcp"),
                addr: target,
            },
            payload,
        }
    }
}

pub enum Message {
    OpenStream(OpenStreamRequest),
    HealthCheck,
}
impl Message {
    pub fn open_stream(
        stream: tokio::net::TcpStream,
        target: String,
        payload: Option<Vec<u8>>,
    ) -> Message {
        let req = OpenStreamRequest::from(stream, target, payload);
        Message::OpenStream(req)
    }
}

trait MuxConnection {
    type SendStream: AsyncWrite + Unpin + Send;
    type RecvStream: AsyncRead + Unpin + Send;
    async fn ping(&mut self) -> anyhow::Result<()>;
    async fn connect(&mut self, url: &Url, key_path: &PathBuf, host: &String)
        -> anyhow::Result<()>;
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;
    fn is_valid(&self) -> bool;
    // async fn accept_stream(&self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;
}

pub struct QuicInnerConnection {
    inner: Option<quinn::Connection>,
    endpoint: Arc<quinn::Endpoint>,
}

impl MuxConnection for QuicInnerConnection {
    type SendStream = quinn::SendStream;
    type RecvStream = quinn::RecvStream;
    fn is_valid(&self) -> bool {
        !self.inner.is_none()
    }
    async fn ping(&mut self) -> anyhow::Result<()> {
        match &mut self.inner {
            None => Err(anyhow!("null connection")),
            Some(c) => {
                let _ = self.open_stream().await?;
                Ok(())
            }
        }
    }
    async fn connect(
        &mut self,
        url: &Url,
        key_path: &PathBuf,
        host: &String,
    ) -> anyhow::Result<()> {
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
            Some(c) => {
                let (send, recv) = c.open_bi().await.map_err(|e| {
                    tracing::error!("{}", e);
                    // c.close( &[u8; 1]);
                    self.inner = None;
                    anyhow!("failed to open stream: {}", e)
                })?;

                Ok((send, recv))
            }
        }
    }
    // async fn accept_stream(&self) -> anyhow::Result<(quinn::SendStream, quinn::RecvStream)> {
    //     let (send, recv) = self
    //         .inner
    //         .as_ref()
    //         .unwrap()
    //         .accept_bi()
    //         .await
    //         .map_err(|e| anyhow!("failed to accept stream: {}", e))?;

    //     Ok((send, recv))
    // }
}

pub struct TlsInnerConnection {
    inner: Option<mux::Connection>,
    id: u32,
}

impl MuxConnection for TlsInnerConnection {
    type SendStream = tokio::io::WriteHalf<MuxStream>;
    type RecvStream = tokio::io::ReadHalf<MuxStream>;
    fn is_valid(&self) -> bool {
        !self.inner.is_none()
    }

    async fn ping(&mut self) -> anyhow::Result<()> {
        match &mut self.inner {
            None => Err(anyhow!("null connection")),
            Some(c) => c.ping().await,
        }
    }
    async fn connect(
        &mut self,
        url: &Url,
        key_path: &PathBuf,
        host: &String,
    ) -> anyhow::Result<()> {
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

trait MuxClientTrait {
    type SendStream: AsyncWrite + Unpin + Send;
    type RecvStream: AsyncRead + Unpin + Send;
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;
    async fn health_check(&mut self) -> anyhow::Result<()>;
}

pub struct MuxClient<T> {
    url: url::Url,
    conns: Vec<T>,
    host: String,
    cursor: usize,
    cert: Option<PathBuf>,
}

impl<T: MuxConnection> MuxClientTrait for MuxClient<T> {
    type SendStream = T::SendStream;
    type RecvStream = T::RecvStream;
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)> {
        for _i in 0..self.conns.len() {
            let idx = self.cursor % self.conns.len();
            self.cursor += 1;
            if let Ok((send, recv)) = self.conns[idx].open_stream().await {
                return Ok((send, recv));
            }
        }
        Err(anyhow!("no available stream"))
    }
    async fn health_check(&mut self) -> anyhow::Result<()> {
        for c in &mut self.conns {
            if !c.is_valid() {
                if let Err(e) = c
                    .connect(&self.url, self.cert.as_ref().unwrap(), &self.host)
                    .await
                {
                    tracing::error!("reconnect error:{}", e);
                }
            } else {
                if let Err(e) = c.ping().await {
                    tracing::error!("open stream failed:{}", e);
                }
            }
        }
        Ok(())
    }
}

async fn mux_client_loop<T: MuxClientTrait>(
    mut client: T,
    mut receiver: mpsc::UnboundedReceiver<Message>,
) where
    <T as MuxClientTrait>::SendStream: 'static,
    <T as MuxClientTrait>::RecvStream: 'static,
{
    while let Some(msg) = receiver.recv().await {
        match msg {
            Message::OpenStream(mut event) => {
                tracing::info!("Proxy request to {}", event.event.addr);
                if let Ok((mut send, mut recv)) = client.open_stream().await {
                    increment_gauge!("client_proxy_streams", 1.0);
                    tokio::spawn(async move {
                        // tracing::info!("create remote proxy stream success");
                        let (mut local_reader, mut local_writer) = event.tcp_stream.split();
                        let ev = event::new_open_stream_event(0, &event.event);
                        if let Err(e) = event::write_event(&mut send, ev).await {
                            tracing::error!("write open stream event failed:{}", e);
                        } else {
                            if let Some(payload) = event.payload {
                                if let Err(e) = send.write_all(&payload).await {
                                    tracing::error!("write payload failed:{}", e);
                                    return;
                                }
                            }
                            let mut stream = Stream::new(
                                &mut local_reader,
                                &mut local_writer,
                                &mut recv,
                                &mut send,
                            );
                            if let Err(e) = stream.transfer().await {
                                tracing::error!("transfer finish:{}", e);
                            }
                        }
                        decrement_gauge!("client_proxy_streams", 1.0);
                    });
                } else {
                    tracing::error!("create remote proxy stream failed");
                }
            }
            Message::HealthCheck => {
                let _ = client.health_check().await;
            }
        }
    }
}

impl MuxClient<QuicInnerConnection> {
    pub async fn from(
        url: &Url,
        cert_path: &PathBuf,
        host: &String,
        count: usize,
    ) -> anyhow::Result<mpsc::UnboundedSender<Message>> {
        match url.scheme() {
            "quic" => {
                let (sender, receiver) = mpsc::unbounded_channel::<Message>();
                let mut client: MuxClient<QuicInnerConnection> = MuxClient {
                    url: url.clone(),
                    conns: Vec::new(),
                    host: String::from(host),
                    cursor: 0,
                    cert: Some(cert_path.clone()),
                };
                let endpoint = new_quic_endpoint(url, cert_path)?;
                let endpoint = Arc::new(endpoint);
                for i in 0..count {
                    let mut quic_conn = QuicInnerConnection {
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

impl MuxClient<TlsInnerConnection> {
    pub async fn from(
        url: &Url,
        cert_path: &PathBuf,
        host: &String,
        count: usize,
    ) -> anyhow::Result<mpsc::UnboundedSender<Message>> {
        match url.scheme() {
            "tls" => {
                let (sender, receiver) = mpsc::unbounded_channel::<Message>();
                let mut client: MuxClient<TlsInnerConnection> = MuxClient {
                    url: url.clone(),
                    conns: Vec::new(),
                    host: String::from(host),
                    cursor: 0,
                    cert: Some(cert_path.clone()),
                };
                for i in 0..count {
                    let mut tls_conn: TlsInnerConnection = TlsInnerConnection {
                        inner: None,
                        id: i as u32,
                    };
                    match tls_conn.connect(url, cert_path, &host).await {
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
                tokio::spawn(mux_client_loop(client, receiver));
                return Ok(sender);
            }
            _ => {
                return Err(anyhow!("unsupported schema:{:?}", url.scheme()));
            }
        }
    }
}

fn new_quic_endpoint(url: &Url, cert_path: &PathBuf) -> anyhow::Result<quinn::Endpoint> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(&rustls::Certificate(std::fs::read(cert_path)?))?;
    let mut client_crypto = rustls::ClientConfig::builder()
        .with_safe_defaults()
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
    host: &String,
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

async fn new_tls_connection(
    url: &Url,
    cert_path: &PathBuf,
    domain: &String,
) -> anyhow::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let remote = (url.host_str().unwrap(), url.port().unwrap_or(4433))
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("couldn't resolve to an address"))?;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(&tokio_rustls::rustls::Certificate(std::fs::read(
        cert_path,
    )?))?;
    let mut client_crypto = tokio_rustls::rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(roots)
        .with_no_client_auth();

    client_crypto.alpn_protocols = ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();

    let connector = TlsConnector::from(Arc::new(client_crypto));
    let stream = TcpStream::connect(&remote).await?;

    let domain = tokio_rustls::rustls::ServerName::try_from(domain.as_str())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid dnsname"))?
        .to_owned();

    let stream: tokio_rustls::client::TlsStream<tokio::net::TcpStream> =
        connector.connect(domain, stream).await?;
    Ok(stream)
}
