use anyhow::anyhow;
use metrics::{decrement_gauge, increment_gauge};
use std::any::Any;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use url::Url;

use crate::mux::event;
use crate::mux::event::OpenStreamEvent;

/// Bounded channel capacity for the proxy message queue.
/// Limits memory growth under load via backpressure.
pub const PROXY_CHANNEL_CAPACITY: usize = 256;

/// Type alias for the bounded proxy message sender.
pub type ProxySender = mpsc::Sender<Message>;
/// Type alias for the bounded proxy message receiver.
pub type ProxyReceiver = mpsc::Receiver<Message>;
use crate::tunnel::stream::Stream;
use crate::utils::UdpServerStream;

pub struct OpenStreamRequest {
    tcp_stream: Option<tokio::net::TcpStream>,
    #[allow(dead_code)]
    udp_stream: Option<UdpServerStream>,
    event: OpenStreamEvent,
    payload: Option<Vec<u8>>,
}

impl OpenStreamRequest {
    pub fn from_tcp(
        stream: tokio::net::TcpStream,
        target: String,
        payload: Option<Vec<u8>>,
    ) -> Self {
        Self {
            tcp_stream: Some(stream),
            udp_stream: None,
            event: OpenStreamEvent {
                proto: String::from("tcp"),
                addr: target,
            },
            payload,
        }
    }
    #[allow(dead_code)]
    pub fn from_udp(stream: UdpServerStream, target: String, payload: Option<Vec<u8>>) -> Self {
        Self {
            tcp_stream: None,
            udp_stream: Some(stream),
            event: OpenStreamEvent {
                proto: String::from("udp"),
                addr: target,
            },
            payload,
        }
    }
}

#[allow(dead_code)]
pub enum Message {
    OpenStream(OpenStreamRequest),
    HealthCheck,
    AddConnection(Box<dyn Any + Send + Sync>),
}
impl Message {
    pub fn open_tcp_stream(
        stream: tokio::net::TcpStream,
        target: String,
        payload: Option<Vec<u8>>,
    ) -> Message {
        let req = OpenStreamRequest::from_tcp(stream, target, payload);
        Message::OpenStream(req)
    }

    #[allow(dead_code)]
    pub fn open_udp_stream(
        stream: UdpServerStream,
        target: String,
        payload: Option<Vec<u8>>,
    ) -> Message {
        let req = OpenStreamRequest::from_udp(stream, target, payload);
        Message::OpenStream(req)
    }
}

pub(crate) trait MuxConnection {
    type SendStream: AsyncWrite + Unpin + Send;
    type RecvStream: AsyncRead + Unpin + Send;
    async fn ping(&mut self) -> anyhow::Result<()>;
    async fn connect(&mut self, url: &Url, key_path: &Path, host: &str) -> anyhow::Result<()>;
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;
    async fn accept_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;
    fn is_valid(&self) -> bool;
    fn set_connection(&mut self, new_c: Self);
}

pub(crate) trait MuxClientTrait {
    type SendStream: AsyncWrite + Unpin + Send;
    type RecvStream: AsyncRead + Unpin + Send;
    type Connection;
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;
    async fn health_check(&mut self) -> anyhow::Result<()>;
    fn add_connection(&mut self, c: Self::Connection) -> anyhow::Result<()>;
}

pub(crate) struct MuxClient<T> {
    pub(crate) url: url::Url,
    pub(crate) conns: Vec<T>,
    pub(crate) host: String,
    pub(crate) cursor: usize,
    pub(crate) cert: Option<PathBuf>,
}

impl<T: MuxConnection> MuxClientTrait for MuxClient<T> {
    type SendStream = T::SendStream;
    type RecvStream = T::RecvStream;
    type Connection = T;
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
            } else if let Err(e) = c.ping().await {
                tracing::error!("open stream failed:{}", e);
            }
        }
        Ok(())
    }

    fn add_connection(&mut self, new_c: Self::Connection) -> anyhow::Result<()> {
        for c in &mut self.conns {
            if !c.is_valid() {
                c.set_connection(new_c);
                return Ok(());
            }
        }
        self.conns.push(new_c);
        Ok(())
    }
}

pub(crate) async fn mux_client_loop<T: MuxClientTrait>(
    mut client: T,
    mut receiver: ProxyReceiver,
    idle_timeout_secs: usize,
) where
    <T as MuxClientTrait>::SendStream: 'static,
    <T as MuxClientTrait>::RecvStream: 'static,
    <T as MuxClientTrait>::Connection: 'static,
{
    while let Some(msg) = receiver.recv().await {
        match msg {
            Message::OpenStream(event) => {
                tracing::info!("Proxy request to {}", event.event.addr);
                if let Ok((mut send, mut recv)) = client.open_stream().await {
                    increment_gauge!("client_proxy_streams", 1.0);
                    tokio::spawn(async move {
                        if let Some(mut tcp_stream) = event.tcp_stream {
                            let (mut local_reader, mut local_writer) = tcp_stream.split();
                            let ev = match event::new_open_stream_event(0, &event.event) {
                                Ok(ev) => ev,
                                Err(e) => {
                                    tracing::error!("create open stream event failed:{}", e);
                                    decrement_gauge!("client_proxy_streams", 1.0);
                                    return;
                                }
                            };
                            if let Err(e) = event::write_event(&mut send, ev).await {
                                tracing::error!("write open stream event failed:{}", e);
                                decrement_gauge!("client_proxy_streams", 1.0);
                                return;
                            }
                            if let Some(payload) = event.payload {
                                if let Err(e) = send.write_all(&payload).await {
                                    tracing::error!("write payload failed:{}", e);
                                    decrement_gauge!("client_proxy_streams", 1.0);
                                    return;
                                }
                            }
                            let mut stream = Stream::new(
                                &mut local_reader,
                                &mut local_writer,
                                &mut recv,
                                &mut send,
                            );
                            if let Err(e) = stream.transfer(idle_timeout_secs).await {
                                tracing::debug!("transfer finish:{}", e);
                            }
                        } else if let Some(udp_stream) = event.udp_stream {
                            let (mut local_reader, mut local_writer) = tokio::io::split(udp_stream);
                            let ev = match event::new_open_stream_event(0, &event.event) {
                                Ok(ev) => ev,
                                Err(e) => {
                                    tracing::error!("create open stream event failed:{}", e);
                                    decrement_gauge!("client_proxy_streams", 1.0);
                                    return;
                                }
                            };
                            if let Err(e) = event::write_event(&mut send, ev).await {
                                tracing::error!("write open stream event failed:{}", e);
                                decrement_gauge!("client_proxy_streams", 1.0);
                                return;
                            }
                            if let Some(payload) = event.payload {
                                if let Err(e) = send.write_all(&payload).await {
                                    tracing::error!("write payload failed:{}", e);
                                    decrement_gauge!("client_proxy_streams", 1.0);
                                    return;
                                }
                            }
                            let mut stream = Stream::new(
                                &mut local_reader,
                                &mut local_writer,
                                &mut recv,
                                &mut send,
                            );
                            if let Err(e) = stream.transfer(idle_timeout_secs).await {
                                tracing::debug!("transfer finish:{}", e);
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
            Message::AddConnection(c) => {
                match c.downcast::<T::Connection>().ok() {
                    Some(obj) => {
                        let _ = client.add_connection(*obj);
                    }
                    None => {
                        tracing::error!("AddConnection failed: connection type mismatch");
                    }
                }
            }
        }
    }
}
