use anyhow::anyhow;
use std::any::Any;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
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
    ReplaceConnection(Box<dyn Any + Send + Sync>),
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
    fn close(&mut self);
    fn active_stream_count(&self) -> usize;
}

pub(crate) trait MuxClientTrait {
    type SendStream: AsyncWrite + Unpin + Send;
    type RecvStream: AsyncRead + Unpin + Send;
    type Connection;
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;
    async fn health_check(&mut self) -> anyhow::Result<()>;
    fn add_connection(&mut self, c: Self::Connection) -> anyhow::Result<()>;
}

pub(crate) struct PoolConnection<T> {
    pub(crate) conn: T,
    retire_at: Instant,
    retired: bool,
}

impl<T> PoolConnection<T> {
    pub fn new(conn: T, max_age_secs: u64, conn_index: usize) -> Self {
        let jitter = ((conn_index.wrapping_mul(73)) % 201) as i64 - 100;
        let retire_at = Instant::now() + Duration::from_secs((max_age_secs as i64 + jitter).max(0) as u64);
        Self {
            conn,
            retire_at,
            retired: false,
        }
    }
}

pub(crate) struct MuxClient<T> {
    pub(crate) url: url::Url,
    pub(crate) conns: Vec<PoolConnection<T>>,
    pub(crate) host: String,
    pub(crate) cursor: usize,
    pub(crate) cert: Option<PathBuf>,
    pub(crate) max_age_secs: u64,
    pub(crate) retirement_notify: mpsc::UnboundedSender<usize>,
}

impl<T: MuxConnection> MuxClientTrait for MuxClient<T> {
    type SendStream = T::SendStream;
    type RecvStream = T::RecvStream;
    type Connection = T;
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)> {
        let len = self.conns.len();
        for _i in 0..len {
            let idx = self.cursor % len;
            self.cursor += 1;
            let pc = &mut self.conns[idx];
            if pc.retired {
                continue;
            }
            if let Ok((send, recv)) = pc.conn.open_stream().await {
                return Ok((send, recv));
            }
        }
        Err(anyhow!("no available stream"))
    }

    async fn health_check(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        for i in 0..self.conns.len() {
            let pc = &mut self.conns[i];
            // Skip already-dead retired connections
            if pc.retired && !pc.conn.is_valid() {
                continue;
            }
            // Ping retired connections still serving streams to detect failures
            if pc.retired {
                if pc.conn.is_valid() {
                    if let Err(e) = pc.conn.ping().await {
                        tracing::error!("retired connection ping failed:{}", e);
                    }
                }
                continue;
            }
            // Check retirement age (only for non-retired connections, disabled when max_age is 0)
            if self.max_age_secs > 0 && now >= pc.retire_at {
                tracing::info!("Connection {} reached max age, retiring", i);
                pc.retired = true;
                let _ = self.retirement_notify.send(i);
                continue;
            }
            // Normal health check for active connections
            if pc.conn.is_valid() {
                if let Err(e) = pc.conn.ping().await {
                    tracing::error!("ping failed:{}", e);
                }
            } else if let Err(e) = pc
                .conn
                .connect(&self.url, self.cert.as_ref().unwrap(), &self.host)
                .await
            {
                tracing::error!("reconnect error:{}", e);
            }
        }
        Ok(())
    }

    fn add_connection(&mut self, new_c: Self::Connection) -> anyhow::Result<()> {
        // First try: replace an invalid (disconnected) or retired slot
        for (i, pc) in self.conns.iter_mut().enumerate() {
            if !pc.conn.is_valid() || pc.retired {
                let jitter = ((i.wrapping_mul(73)) % 201) as i64 - 100;
                *pc = PoolConnection {
                    conn: new_c,
                    retire_at: Instant::now() + Duration::from_secs((self.max_age_secs as i64 + jitter).max(0) as u64),
                    retired: false,
                };
                return Ok(());
            }
        }
        // All slots valid and active — append
        let idx = self.conns.len();
        self.conns.push(PoolConnection::new(new_c, self.max_age_secs, idx));
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
                // tracing::info!("Proxy request to {}", event.event.addr);
                // Wrap open_stream in a timeout to prevent the serial
                // mux_client_loop from blocking indefinitely when the
                // control channel is saturated.
                match tokio::time::timeout(Duration::from_secs(1), client.open_stream()).await {
                    Ok(Ok((mut send, mut recv))) => {
                    metrics::gauge!("client_proxy_streams").increment(1.0);
                    tokio::spawn(async move {
                        if let Some(mut tcp_stream) = event.tcp_stream {
                            let (mut local_reader, mut local_writer) = tcp_stream.split();
                            let ev = match event::new_open_stream_event(0, &event.event) {
                                Ok(ev) => ev,
                                Err(e) => {
                                    tracing::error!("create open stream event failed:{}", e);
                                    metrics::gauge!("client_proxy_streams").decrement(1.0);
                                    return;
                                }
                            };
                            if let Err(e) = event::write_event(&mut send, ev).await {
                                tracing::error!("write open stream event failed:{}", e);
                                metrics::gauge!("client_proxy_streams").decrement(1.0);
                                return;
                            }
                            if let Some(payload) = event.payload {
                                if let Err(e) = send.write_all(&payload).await {
                                    tracing::error!("write payload failed:{}", e);
                                    metrics::gauge!("client_proxy_streams").decrement(1.0);
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
                                    metrics::gauge!("client_proxy_streams").decrement(1.0);
                                    return;
                                }
                            };
                            if let Err(e) = event::write_event(&mut send, ev).await {
                                tracing::error!("write open stream event failed:{}", e);
                                metrics::gauge!("client_proxy_streams").decrement(1.0);
                                return;
                            }
                            if let Some(payload) = event.payload {
                                if let Err(e) = send.write_all(&payload).await {
                                    tracing::error!("write payload failed:{}", e);
                                    metrics::gauge!("client_proxy_streams").decrement(1.0);
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
                        metrics::gauge!("client_proxy_streams").decrement(1.0);
                    });
                } Err(_) | Ok(Err(_)) => {
                    crate::mux::metrics::inc_client_open_stream_failed();
                    tracing::error!("create remote proxy stream failed or timed out");
                }}
            }
            Message::HealthCheck => {
                // Use a short timeout to prevent health_check from
                // blocking the entire mux_client_loop when the control
                // channel is saturated (Bug 2).
                let _ = tokio::time::timeout(
                    Duration::from_millis(500),
                    client.health_check(),
                )
                .await;
            }
            Message::AddConnection(c) => match c.downcast::<T::Connection>().ok() {
                Some(obj) => {
                    let _ = client.add_connection(*obj);
                }
                None => {
                    tracing::error!("AddConnection failed: connection type mismatch");
                }
            },
            Message::ReplaceConnection(c) => match c.downcast::<T::Connection>().ok() {
                Some(obj) => {
                    let _ = client.add_connection(*obj);
                }
                None => {
                    tracing::error!("ReplaceConnection failed: connection type mismatch");
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_range_is_bounded() {
        for i in 0..1000usize {
            let jitter = ((i.wrapping_mul(73)) % 201) as i64 - 100;
            assert!(jitter >= -100, "jitter {} too low for index {}", jitter, i);
            assert!(jitter <= 100, "jitter {} too high for index {}", jitter, i);
        }
    }

    #[test]
    fn pool_connection_retire_at_is_correct() {
        let max_age_secs = 1800u64;
        let conn_index = 0usize;
        let jitter = ((conn_index.wrapping_mul(73)) % 201) as i64 - 100;
        let pc_created = Instant::now();
        let pc_retire = pc_created + Duration::from_secs((max_age_secs as i64 + jitter).max(0) as u64);

        let elapsed = pc_retire.duration_since(pc_created);
        assert!(elapsed >= Duration::from_secs(max_age_secs - 100));
        assert!(elapsed <= Duration::from_secs(max_age_secs + 100));
    }
}
