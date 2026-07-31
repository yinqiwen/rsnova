use anyhow::{Result, anyhow};

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf, copy_bidirectional_with_sizes,
};
use tokio::time::{sleep, timeout};

use crate::mux::event::{self, OpenStreamAck, OpenStreamError, OpenStreamEvent};
use crate::tunnel::CHECK_TIMEOUT_SECS;
use crate::tunnel::DEFAULT_TIMEOUT_SECS;
use crate::utils::UdpClientStream;

/// Per-direction relay buffer. 16KB halves the resident memory of each
/// proxied stream versus 32KB with no measurable throughput cost: the mux
/// stream window (256KB by default) absorbs the smaller read chunks, and
/// `poll_write` already splits writes at the flow-control window. See the
/// `mux_stream_write/32768` benchmark — 16KB vs 32KB relay buffers are
/// within noise on a 28 MiB/s write path.
const TRANSFER_BUF_SIZE: usize = 16 * 1024;

/// Connection-level last-activity tracker shared by both directions.
///
/// Only `touch()` writes; idle checks are read-only. This avoids the previous
/// `AtomicBool::swap(false)` design where each direction's idle timer could
/// clear the other direction's activity credit and false-trigger timeouts
/// during one-way downloads.
struct IdleTrack {
    start: Instant,
    last_active_millis: AtomicU64,
}

impl IdleTrack {
    fn new() -> Self {
        let track = Self {
            start: Instant::now(),
            last_active_millis: AtomicU64::new(0),
        };
        track.touch();
        track
    }

    #[inline]
    fn touch(&self) {
        self.last_active_millis
            .store(self.start.elapsed().as_millis() as u64, Relaxed);
    }

    fn idle_millis(&self) -> u64 {
        let now = self.start.elapsed().as_millis() as u64;
        now.saturating_sub(self.last_active_millis.load(Relaxed))
    }
}

/// Thin `AsyncRead`/`AsyncWrite` wrapper that records byte-level progress on
/// a shared [`IdleTrack`]. Pending I/O does **not** count as activity.
struct IdleStream<'a, S: ?Sized> {
    inner: &'a mut S,
    track: Arc<IdleTrack>,
}

impl<S: AsyncRead + Unpin + ?Sized> AsyncRead for IdleStream<'_, S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut *self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if buf.filled().len() > before {
                    self.track.touch();
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: AsyncWrite + Unpin + ?Sized> AsyncWrite for IdleStream<'_, S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut *self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                if n > 0 {
                    self.track.touch();
                }
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut *self.inner).poll_write_vectored(cx, bufs) {
            Poll::Ready(Ok(n)) => {
                if n > 0 {
                    self.track.touch();
                }
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

async fn copy_bidirectional_with_idle<A, B>(
    a: &mut A,
    b: &mut B,
    idle_timeout_secs: u64,
) -> Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    if idle_timeout_secs == 0 {
        copy_bidirectional_with_sizes(a, b, TRANSFER_BUF_SIZE, TRANSFER_BUF_SIZE).await?;
        metrics::counter!("mux.stream.close.eof").increment(1);
        return Ok(());
    }

    let track = Arc::new(IdleTrack::new());
    let mut a = IdleStream {
        inner: a,
        track: track.clone(),
    };
    let mut b = IdleStream {
        inner: b,
        track: track.clone(),
    };

    let copy = copy_bidirectional_with_sizes(&mut a, &mut b, TRANSFER_BUF_SIZE, TRANSFER_BUF_SIZE);
    tokio::pin!(copy);

    let idle_limit_millis = idle_timeout_secs.saturating_mul(1000);
    let checker = sleep(Duration::from_secs(CHECK_TIMEOUT_SECS));
    tokio::pin!(checker);

    loop {
        tokio::select! {
            result = &mut copy => {
                match result {
                    Ok(_) => {
                        metrics::counter!("mux.stream.close.eof").increment(1);
                        return Ok(());
                    }
                    Err(e) => {
                        metrics::counter!("mux.stream.close.read_error").increment(1);
                        return Err(e.into());
                    }
                }
            }
            _ = &mut checker => {
                if track.idle_millis() >= idle_limit_millis {
                    metrics::counter!("mux.stream.close.idle_timeout").increment(1);
                    return Err(anyhow!(
                        "idle timeout: no activity for {}s",
                        idle_timeout_secs
                    ));
                }
                checker
                    .as_mut()
                    .reset(tokio::time::Instant::now() + Duration::from_secs(CHECK_TIMEOUT_SECS));
            }
        }
    }
}

pub struct Stream<'a, LR, LW, RR, RW> {
    local_reader: &'a mut LR,
    local_writer: &'a mut LW,
    remote_reader: &'a mut RR,
    remote_writer: &'a mut RW,
}

impl<'a, LR, LW, RR, RW> Stream<'a, LR, LW, RR, RW>
where
    LR: AsyncReadExt + Unpin,
    LW: AsyncWriteExt + Unpin,
    RR: AsyncReadExt + Unpin,
    RW: AsyncWriteExt + Unpin,
{
    pub fn new(lr: &'a mut LR, lw: &'a mut LW, rr: &'a mut RR, rw: &'a mut RW) -> Self {
        Self {
            local_reader: lr,
            local_writer: lw,
            remote_reader: rr,
            remote_writer: rw,
        }
    }

    pub async fn transfer(&mut self, idle_timeout_secs: usize) -> Result<()> {
        let mut local = tokio::io::join(&mut *self.local_reader, &mut *self.local_writer);
        let mut remote = tokio::io::join(&mut *self.remote_reader, &mut *self.remote_writer);
        copy_bidirectional_with_idle(&mut local, &mut remote, idle_timeout_secs as u64).await
    }
}

pub async fn handle_server_stream<'a, LR: AsyncReadExt + Unpin, LW: AsyncWriteExt + Unpin>(
    mut lr: &'a mut LR,
    lw: &'a mut LW,
    idle_timeout_secs: usize,
) -> Result<()> {
    let timeout_secs = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    match timeout(timeout_secs, event::read_event(&mut lr)).await? {
        Err(e) => match e.kind() {
            std::io::ErrorKind::UnexpectedEof => Ok(()),
            _ => Err(anyhow::Error::new(e)),
        },
        Ok(ev) => {
            if ev.header.flags() != event::FLAG_OPEN {
                return Err(anyhow!("unexpected flag:{}", ev.header.flags()));
            }
            let config = bincode::config::standard();
            let (open_event, _len): (OpenStreamEvent, usize) =
                bincode::decode_from_slice(ev.body.as_ref(), config)?;
            tracing::info!("[{}]recv open event:{:?}", ev.header.stream_id, open_event);
            if open_event.proto == crate::mux::event::StreamProto::Udp {
                let udp_socket = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
                    Ok(socket) => socket,
                    Err(e) => {
                        send_open_ack(lw, OpenStreamAck::failure(map_open_error(&e))).await?;
                        return Ok(());
                    }
                };
                if let Err(e) = udp_socket.connect(&open_event.addr).await {
                    send_open_ack(lw, OpenStreamAck::failure(map_open_error(&e))).await?;
                    return Ok(());
                }
                send_open_ack(lw, OpenStreamAck::success()).await?;
                let udp_stream = UdpClientStream::new(udp_socket);
                let (mut remote_receiver, mut remote_sender) = tokio::io::split(udp_stream);
                let mut stream = Stream::new(lr, lw, &mut remote_receiver, &mut remote_sender);
                stream.transfer(idle_timeout_secs).await
            } else {
                let mut remote_stream = match timeout(
                    timeout_secs,
                    tokio::net::TcpStream::connect(&open_event.addr),
                )
                .await
                {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(e)) => {
                        send_open_ack(lw, OpenStreamAck::failure(map_open_error(&e))).await?;
                        return Ok(());
                    }
                    Err(_) => {
                        send_open_ack(lw, OpenStreamAck::failure(OpenStreamError::TimedOut))
                            .await?;
                        return Ok(());
                    }
                };
                send_open_ack(lw, OpenStreamAck::success()).await?;
                let (mut remote_receiver, mut remote_sender) = remote_stream.split();
                let mut stream = Stream::new(lr, lw, &mut remote_receiver, &mut remote_sender);
                stream.transfer(idle_timeout_secs).await
            }
        }
    }
}

async fn send_open_ack<W: AsyncWriteExt + Unpin>(writer: &mut W, ack: OpenStreamAck) -> Result<()> {
    event::write_event(writer, event::new_open_ack_event(0, &ack)?).await?;
    writer.flush().await?;
    Ok(())
}

fn map_open_error(error: &std::io::Error) -> OpenStreamError {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::ConnectionRefused => OpenStreamError::ConnectionRefused,
        ErrorKind::HostUnreachable => OpenStreamError::HostUnreachable,
        ErrorKind::NetworkUnreachable => OpenStreamError::NetworkUnreachable,
        ErrorKind::TimedOut => OpenStreamError::TimedOut,
        ErrorKind::InvalidInput | ErrorKind::AddrNotAvailable => OpenStreamError::AddressInvalid,
        ErrorKind::OutOfMemory => OpenStreamError::ResourceExhausted,
        _ => OpenStreamError::Other(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[tokio::test]
    async fn idle_timeout_fires_when_both_sides_stall() {
        let (client, _client_peer) = duplex(1024);
        let (remote, _remote_peer) = duplex(1024);
        let (mut lr, mut lw) = tokio::io::split(client);
        let (mut rr, mut rw) = tokio::io::split(remote);

        let start = Instant::now();
        let mut stream = Stream::new(&mut lr, &mut lw, &mut rr, &mut rw);
        let result = stream.transfer(1).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected idle timeout error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("idle timeout"),
            "expected idle timeout message, got: {}",
            err
        );
        assert!(
            elapsed >= Duration::from_millis(900),
            "returned too early: {:?}",
            elapsed
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "took too long: {:?}",
            elapsed
        );
    }

    /// Regression: one-way download must not idle-timeout while bytes keep flowing.
    ///
    /// The previous AtomicBool design let the silent upload direction clear the
    /// shared activity flag every CHECK_TIMEOUT_SECS, so the download direction
    /// accumulated false idle intervals and died around the configured budget.
    #[tokio::test]
    async fn one_way_download_survives_past_idle_budget() {
        let (client_end, proxy_local) = duplex(4096);
        let (proxy_remote, mut server_end) = duplex(4096);
        let (mut lr, mut lw) = tokio::io::split(proxy_local);
        let (mut rr, mut rw) = tokio::io::split(proxy_remote);

        let producer = tokio::spawn(async move {
            for _ in 0..50 {
                server_end.write_all(&[1u8; 512]).await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            server_end.shutdown().await.unwrap();
        });

        let mut client_end = client_end;
        let consumer = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match client_end.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });

        let mut stream = Stream::new(&mut lr, &mut lw, &mut rr, &mut rw);
        let transfer = stream.transfer(2);
        tokio::pin!(transfer);

        // Idle budget is 2s; after 3s of continuous one-way traffic the relay
        // must still be alive (would have false-triggered under the old design).
        let still_running = timeout(Duration::from_secs(3), &mut transfer).await;
        assert!(
            still_running.is_err(),
            "transfer ended before idle budget under one-way load: {:?}",
            still_running.map(|r| r.map_err(|e| e.to_string()))
        );

        producer.abort();
        consumer.abort();
        let _ = producer.await;
        let _ = consumer.await;
    }

    #[test]
    fn idle_track_touch_is_monotonic_and_non_destructive() {
        let track = IdleTrack::new();
        assert!(track.idle_millis() < 50);

        // Read-only checks must not clear activity.
        let _ = track.idle_millis();
        let _ = track.idle_millis();
        assert!(track.idle_millis() < 50);

        std::thread::sleep(Duration::from_millis(20));
        track.touch();
        assert!(track.idle_millis() < 20);
    }
}
