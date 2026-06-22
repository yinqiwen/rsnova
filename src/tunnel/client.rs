use anyhow::anyhow;
use std::any::Any;
use std::path::Path;
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
    #[allow(dead_code)]
    fn set_connection(&mut self, new_c: Self);
    fn close(&mut self);
    #[allow(dead_code)]
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

/// Deterministic jitter for connection retirement, derived from connection index.
/// Range: [-100, +100] seconds. Spreads retirements across a 200-second window.
pub(crate) fn retirement_jitter_secs(index: usize) -> i64 {
    ((index.wrapping_mul(73)) % 201) as i64 - 100
}

pub(crate) struct PoolConnection<T> {
    pub(crate) conn: T,
    retire_at: Instant,
    retired: bool,
}

impl<T> PoolConnection<T> {
    pub fn new(conn: T, max_age_secs: u64, conn_index: usize) -> Self {
        let jitter = retirement_jitter_secs(conn_index);
        let retire_at = Instant::now() + Duration::from_secs((max_age_secs as i64 + jitter).max(0) as u64);
        Self {
            conn,
            retire_at,
            retired: false,
        }
    }
}

pub(crate) struct MuxClient<T> {
    pub(crate) conns: Vec<PoolConnection<T>>,
    pub(crate) cursor: usize,
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
            // open_stream failed — the connection is now invalid and will be
            // retired on the next health_check cycle.
            tracing::warn!(
                "open_stream failed on connection {} (valid={}), will be retired on next health check",
                idx,
                pc.conn.is_valid(),
            );
        }
        // All connections failed — log pool state for diagnosis
        let active = self.conns.iter().filter(|c| !c.retired && c.conn.is_valid()).count();
        let retired_valid = self.conns.iter().filter(|c| c.retired && c.conn.is_valid()).count();
        let dead = self.conns.iter().filter(|c| !c.conn.is_valid()).count();
        tracing::error!(
            "no available stream: pool size={}, active={}, retired+serving={}, dead={}",
            len, active, retired_valid, dead,
        );
        Err(anyhow!("no available stream"))
    }

    async fn health_check(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        for i in 0..self.conns.len() {
            let pc = &mut self.conns[i];
            if pc.retired && !pc.conn.is_valid() {
                // Dead retired connection still waiting for replacement.
                // Re-send retirement notification to ensure a replacement
                // is always in flight. The retirement listener may already
                // have a replacement attempt running for this slot; that's
                // acceptable — duplicate attempts are caught by add_connection
                // replacing the first matching slot, and extra connections
                // simply append to the pool (up to MAX_POOL_SIZE).
                let _ = self.retirement_notify.send(i);
                continue;
            }
            // Ping retired connections still serving streams to detect failures.
            if pc.retired {
                if pc.conn.is_valid() {
                    if let Err(e) = pc.conn.ping().await {
                        tracing::error!("retired connection {} ping failed: {}", i, e);
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
                    tracing::error!("ping failed on connection {}: {}, marking for replacement", i, e);
                    pc.retired = true;
                    let _ = self.retirement_notify.send(i);
                }
            } else {
                tracing::warn!("Connection {} is invalid, retiring and requesting replacement", i);
                pc.retired = true;
                let _ = self.retirement_notify.send(i);
            }
        }
        Ok(())
    }

    fn add_connection(&mut self, new_c: Self::Connection) -> anyhow::Result<()> {
        // First try: replace an invalid (disconnected) or retired slot
        for (i, pc) in self.conns.iter_mut().enumerate() {
            if !pc.conn.is_valid() || pc.retired {
                // Gracefully close the old connection before replacing it.
                // This ensures the mux dispatcher task exits promptly and
                // existing streams receive proper EOF rather than hanging
                // until a TCP keepalive fires.
                pc.conn.close();
                let jitter = retirement_jitter_secs(i);
                tracing::info!(
                    "Replacing connection {} (valid={}, retired={})",
                    i,
                    pc.conn.is_valid(),
                    pc.retired,
                );
                *pc = PoolConnection {
                    conn: new_c,
                    retire_at: Instant::now() + Duration::from_secs((self.max_age_secs as i64 + jitter).max(0) as u64),
                    retired: false,
                };
                return Ok(());
            }
        }
        // All slots valid and active — append (with pool size limit)
        const MAX_POOL_SIZE: usize = 32;
        if self.conns.len() >= MAX_POOL_SIZE {
            tracing::warn!("Connection pool at maximum size ({}), dropping new connection", MAX_POOL_SIZE);
            return Ok(());
        }
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
                // Timeout to prevent health_check from blocking the
                // mux_client_loop indefinitely. Since we no longer
                // reconnect inline (which is unreliable under short
                // timeouts), the main cost is pinging each connection.
                // A 5-second budget is generous for typical pool sizes
                // (4-8 connections with 2-second ping timeout each).
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
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
            let jitter = retirement_jitter_secs(i);
            assert!(jitter >= -100, "jitter {} too low for index {}", jitter, i);
            assert!(jitter <= 100, "jitter {} too high for index {}", jitter, i);
        }
    }

    #[test]
    fn pool_connection_retire_at_is_correct() {
        let max_age_secs = 1800u64;
        let conn_index = 0usize;
        let jitter = retirement_jitter_secs(conn_index);
        let pc_created = Instant::now();
        let pc_retire = pc_created + Duration::from_secs((max_age_secs as i64 + jitter).max(0) as u64);

        let elapsed = pc_retire.duration_since(pc_created);
        assert!(elapsed >= Duration::from_secs(max_age_secs - 100));
        assert!(elapsed <= Duration::from_secs(max_age_secs + 100));
    }

    #[test]
    fn retirement_jitter_uses_shared_function() {
        // Verify the shared function matches the inline formula
        for i in 0..100usize {
            let expected = ((i.wrapping_mul(73)) % 201) as i64 - 100;
            assert_eq!(retirement_jitter_secs(i), expected);
        }
    }

    /// Mock MuxConnection for testing pool behavior.
    struct MockConnection {
        valid: bool,
    }

    impl MuxConnection for MockConnection {
        type SendStream = tokio::io::DuplexStream;
        type RecvStream = tokio::io::DuplexStream;
        async fn ping(&mut self) -> anyhow::Result<()> {
            if self.valid { Ok(()) } else { Err(anyhow!("invalid")) }
        }
        async fn connect(&mut self, _url: &Url, _key_path: &Path, _host: &str) -> anyhow::Result<()> {
            self.valid = true;
            Ok(())
        }
        async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)> {
            if self.valid {
                let (a, b) = tokio::io::duplex(1024);
                Ok((a, b))
            } else {
                Err(anyhow!("invalid"))
            }
        }
        async fn accept_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)> {
            Err(anyhow!("not implemented"))
        }
        fn is_valid(&self) -> bool { self.valid }
        fn set_connection(&mut self, new_c: Self) { *self = new_c; }
        fn close(&mut self) { self.valid = false; }
        fn active_stream_count(&self) -> usize { 0 }
    }

    fn make_client(max_age_secs: u64) -> (MuxClient<MockConnection>, mpsc::UnboundedReceiver<usize>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let client = MuxClient {
            conns: Vec::new(),
            cursor: 0,
            max_age_secs,
            retirement_notify: tx,
        };
        (client, rx)
    }

    #[tokio::test]
    async fn open_stream_skips_retired() {
        let (mut client, _rx) = make_client(1800);
        client.conns.push(PoolConnection {
            conn: MockConnection { valid: true },
            retire_at: Instant::now() + Duration::from_secs(3600),
            retired: true, // retired
        });
        client.conns.push(PoolConnection {
            conn: MockConnection { valid: true },
            retire_at: Instant::now() + Duration::from_secs(3600),
            retired: false,
        });
        // Should skip retired conn[0] and use conn[1]
        let result = client.open_stream().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn open_stream_fails_when_all_retired() {
        let (mut client, _rx) = make_client(1800);
        client.conns.push(PoolConnection {
            conn: MockConnection { valid: true },
            retire_at: Instant::now() + Duration::from_secs(3600),
            retired: true,
        });
        let result = client.open_stream().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn add_connection_replaces_retired_slot() {
        let (mut client, _rx) = make_client(1800);
        client.conns.push(PoolConnection {
            conn: MockConnection { valid: true },
            retire_at: Instant::now() + Duration::from_secs(3600),
            retired: true,
        });
        let new_conn = MockConnection { valid: true };
        client.add_connection(new_conn).unwrap();
        assert_eq!(client.conns.len(), 1);
        assert!(!client.conns[0].retired);
        assert!(client.conns[0].conn.is_valid());
    }

    #[tokio::test]
    async fn add_connection_replaces_invalid_slot() {
        let (mut client, _rx) = make_client(1800);
        client.conns.push(PoolConnection {
            conn: MockConnection { valid: false },
            retire_at: Instant::now() + Duration::from_secs(3600),
            retired: false,
        });
        let new_conn = MockConnection { valid: true };
        client.add_connection(new_conn).unwrap();
        assert_eq!(client.conns.len(), 1);
        assert!(client.conns[0].conn.is_valid());
    }

    #[tokio::test]
    async fn max_age_zero_disables_retirement() {
        let (mut client, mut rx) = make_client(0); // disabled
        client.conns.push(PoolConnection {
            conn: MockConnection { valid: true },
            retire_at: Instant::now(), // already past
            retired: false,
        });
        client.health_check().await.unwrap();
        // Should NOT have retired since max_age is 0
        assert!(!client.conns[0].retired);
        assert!(rx.try_recv().is_err()); // no notification
    }

    #[tokio::test]
    async fn health_check_retires_aged_connection() {
        let (mut client, mut rx) = make_client(1);
        client.conns.push(PoolConnection {
            conn: MockConnection { valid: true },
            retire_at: Instant::now() - Duration::from_secs(1), // already expired
            retired: false,
        });
        client.health_check().await.unwrap();
        assert!(client.conns[0].retired);
        assert!(rx.try_recv().is_ok()); // notification sent
    }

    #[tokio::test]
    async fn pool_size_limit_prevents_growth() {
        let (mut client, _rx) = make_client(1800);
        // Fill pool to max
        for i in 0..32 {
            client.conns.push(PoolConnection::new(
                MockConnection { valid: true },
                1800,
                i,
            ));
        }
        // Try to add one more — should be silently dropped
        let new_conn = MockConnection { valid: true };
        client.add_connection(new_conn).unwrap();
        assert_eq!(client.conns.len(), 32);
    }
}
