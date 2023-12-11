use anyhow::anyhow;
use metrics::{decrement_gauge, increment_gauge};
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use url::Url;

use crate::mux::event;
use crate::mux::event::OpenStreamEvent;
use crate::tunnel::stream::Stream;

pub struct OpenStreamRequest {
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

pub(crate) trait MuxConnection {
    type SendStream: AsyncWrite + Unpin + Send;
    type RecvStream: AsyncRead + Unpin + Send;
    async fn ping(&mut self) -> anyhow::Result<()>;
    async fn connect(&mut self, url: &Url, key_path: &PathBuf, host: &String)
        -> anyhow::Result<()>;
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;
    fn is_valid(&self) -> bool;
    // async fn accept_stream(&self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;
}

pub(crate) trait MuxClientTrait {
    type SendStream: AsyncWrite + Unpin + Send;
    type RecvStream: AsyncRead + Unpin + Send;
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;
    async fn health_check(&mut self) -> anyhow::Result<()>;
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

pub(crate) async fn mux_client_loop<T: MuxClientTrait>(
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
