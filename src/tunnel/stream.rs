use anyhow::{Result, anyhow};

use futures::future::try_join;
use futures::ready;

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Poll};
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::time::Sleep;
use tokio::time::sleep;
use tokio::time::timeout;

use crate::mux::event::{self, OpenStreamEvent};
use crate::tunnel::CHECK_TIMEOUT_SECS;
use crate::tunnel::DEFAULT_TIMEOUT_SECS;
use crate::utils::UdpClientStream;

/// Cross-direction shared state for a single `Stream::transfer`.
///
/// `active` is set by whichever direction successfully reads, and cleared by
/// the idle-check timer when it fires. This replaces the previous
/// `last_active_millis: AtomicU64` + `Instant::elapsed()` per-read syscall
/// — at 30k reads/s/stream that syscall (~25ns each) added up under
/// concurrency. An `AtomicBool` store is ~1ns on x86 and carries no clock
/// dependency.
///
/// Trade-off: idle precision is now `CHECK_TIMEOUT_SECS` (1s) rather than
/// sub-millisecond. That's fine — the idle budget itself is 30s, so 1s
/// granularity is well within the slack we'd want anyway.
struct TransferState {
    abort: AtomicBool,
    active: AtomicBool,
}

impl TransferState {
    fn new() -> Self {
        Self {
            abort: AtomicBool::new(false),
            active: AtomicBool::new(true),
        }
    }
    /// Mark this transfer as having seen progress. Cheaper than recording a
    /// timestamp — just a relaxed atomic store.
    #[inline]
    fn mark_active(&self) {
        self.active.store(true, Relaxed);
    }
    /// Called from the idle-check timer. Returns `true` if the transfer has
    /// seen activity since the last check (and clears the flag for the next
    /// interval); returns `false` if the transfer was idle for a full
    /// interval.
    fn check_and_reset_active(&self) -> bool {
        // `swap` is used instead of `load`+`store` to avoid a TOCTOU where
        // a read landing between load and store could be missed.
        self.active.swap(false, Relaxed)
    }
}

/// Owned transfer buffer modelled on tokio's `io::util::copy::CopyBuffer`.
///
/// Borrowed improvements over the previous `read(&mut [u8; 32768])` +
/// `write_all` loop:
///
/// - Heap-allocated `Box<[u8]>` (borrowed from tokio) instead of a 32 KB stack
///   array. Under 1024 concurrent streams this keeps task stacks small.
/// - When the writer returns `Pending`, we top up the read buffer if there is
///   room (tokio's `poll_write_buf`), so the next write can be a larger,
///   cheaper vectored write instead of a tight write-all loop that idles the
///   reader while waiting on flow control.
/// - When the reader returns `Pending` but the buffer still has unwritten
///   data, we keep draining the writer (and flush if needed) rather than
///   parking the whole task — this avoids a deadlock when the reader depends
///   on the writer making progress (e.g. mux flow control echo).
/// - `poll_write` returning 0 is treated as `WriteZero` (tokio's contract)
///   rather than silently looping.
///
/// Preserved project-specific behaviour:
/// - `idle_timeout_secs` soft polling: a `Sleep` armed for `CHECK_TIMEOUT_SECS`
///   fires periodically; on each fire we check the shared `active` flag. If
///   the flag is set we clear it and re-arm; if it is clear we increment an
///   idle-interval counter, and once that counter reaches
///   `timeout_sec / CHECK_TIMEOUT_SECS` we return an idle-timeout error.
///   This preserves the "continue on transient silence, exit only on budget
///   exceeded" semantics that `tokio::io::copy_bidirectional` cannot express.
/// - `state.abort` flag: a direction observing an error sets abort so the
///   peer direction aborts on its next poll.
/// - `state.mark_active()` on every successful read, plus four
///   `metrics::counter!` close-reason counters.
struct TransferBuffer {
    read_done: bool,
    need_flush: bool,
    pos: usize,
    cap: usize,
    buf: Box<[u8]>,
    /// Sleep armed for `CHECK_TIMEOUT_SECS`. When it fires we re-check idle
    /// budget and re-arm. `Option` because we recreate it on each re-arm.
    idle_check: Option<Pin<Box<Sleep>>>,
    /// Number of consecutive idle intervals (Sleep fires with no read
    /// activity since the previous fire). Reset to 0 whenever `active` was
    /// set. Compared against `timeout_sec / CHECK_TIMEOUT_SECS` to decide
    /// whether the idle budget is exceeded.
    idle_intervals: u32,
}

impl TransferBuffer {
    fn new(buf_size: usize) -> Self {
        Self {
            read_done: false,
            need_flush: false,
            pos: 0,
            cap: 0,
            buf: vec![0u8; buf_size].into_boxed_slice(),
            idle_check: None,
            idle_intervals: 0,
        }
    }

    fn arm_idle_check(&mut self) {
        self.idle_check = Some(Box::pin(sleep(Duration::from_secs(CHECK_TIMEOUT_SECS))));
    }

    fn poll_fill_buf<R>(
        &mut self,
        cx: &mut Context<'_>,
        reader: Pin<&mut R>,
        state: &TransferState,
    ) -> Poll<io::Result<()>>
    where
        R: AsyncRead + ?Sized,
    {
        let me = &mut *self;
        let mut buf = ReadBuf::new(&mut me.buf);
        buf.set_filled(me.cap);
        let res = reader.poll_read(cx, &mut buf);
        if let Poll::Ready(Ok(())) = res {
            let filled_len = buf.filled().len();
            // Only mark active if we actually read new bytes. A zero-length
            // fill (read returning 0 bytes without EOF) shouldn't reset the
            // idle timer.
            if filled_len > me.cap {
                state.mark_active();
            }
            me.read_done = me.cap == filled_len;
            me.cap = filled_len;
        }
        res
    }

    /// Borrowed from tokio: while waiting on the writer, top up the read
    /// buffer if there is spare capacity. This converts "writer stalled, so
    /// reader idles" into "writer stalled, so we read more ahead".
    fn poll_write_buf<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        mut reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
        state: &TransferState,
    ) -> Poll<io::Result<usize>>
    where
        R: AsyncRead + ?Sized,
        W: AsyncWrite + ?Sized,
    {
        let me = &mut *self;
        match writer.as_mut().poll_write(cx, &me.buf[me.pos..me.cap]) {
            Poll::Pending => {
                if !me.read_done && me.cap < me.buf.len() {
                    ready!(me.poll_fill_buf(cx, reader.as_mut(), state))?;
                }
                Poll::Pending
            }
            res => res,
        }
    }

    /// Drive one step of the copy loop. Returns:
    /// - `Poll::Ready(Ok(()))` when EOF reached and flushed.
    /// - `Poll::Ready(Err(_))` on IO error, idle timeout, or abort.
    /// - `Poll::Pending` when waiting on reader/writer.
    fn poll_copy<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        mut reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
        timeout_sec: u64,
        state: &TransferState,
    ) -> Poll<Result<()>>
    where
        R: AsyncRead + ?Sized,
        W: AsyncWrite + ?Sized,
    {
        loop {
            // Check abort flag first — the peer direction may have set it.
            if state.abort.load(Relaxed) {
                metrics::counter!("mux.stream.close.abort").increment(1);
                return Poll::Ready(Err(anyhow!("abort")));
            }

            // Idle-timeout soft polling. The Sleep fires every
            // `CHECK_TIMEOUT_SECS`. On each fire we check the `active` flag.
            // See the struct doc for the full rationale.
            //
            // We poll the Sleep via a temporary match rather than a let-chain
            // so the `&mut self.idle_check` borrow ends before we touch other
            // `self` fields below.
            let idle_fired = match self.idle_check.as_mut() {
                Some(sleep) => matches!(sleep.as_mut().poll(cx), Poll::Ready(())),
                None => false,
            };
            if idle_fired {
                // timeout_sec == 0 disables the idle timeout entirely.
                // Still re-arm the Sleep so the task wakes periodically
                // to poll the abort flag; otherwise a permanently-stalled
                // reader would never notice the peer direction errored.
                if timeout_sec == 0 {
                    self.idle_intervals = 0;
                    self.arm_idle_check();
                    continue;
                }
                if state.check_and_reset_active() {
                    self.idle_intervals = 0;
                } else {
                    self.idle_intervals += 1;
                    let budget_intervals = (timeout_sec / CHECK_TIMEOUT_SECS).max(1) as u32;
                    if self.idle_intervals >= budget_intervals {
                        metrics::counter!("mux.stream.close.idle_timeout").increment(1);
                        return Poll::Ready(Err(anyhow!(
                            "idle timeout: no activity for {}s",
                            timeout_sec
                        )));
                    }
                }
                self.arm_idle_check();
                // Re-arm created a fresh Sleep whose waker is not yet
                // registered with the runtime. `continue` re-enters the loop
                // so the new Sleep gets polled (registering its waker) before
                // this task parks. Without this, a permanently-idle reader
                // would never be woken again.
                continue;
            }

            // Fill phase: if buffer has room and we haven't seen EOF, try to
            // read more. `poll_fill_buf` marks the transfer active on a
            // successful non-zero read.
            if self.cap < self.buf.len() && !self.read_done {
                match self.poll_fill_buf(cx, reader.as_mut(), state) {
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(e)) => {
                        state.abort.store(true, Relaxed);
                        metrics::counter!("mux.stream.close.read_error").increment(1);
                        return Poll::Ready(Err(e.into()));
                    }
                    Poll::Pending => {
                        // Borrowed from tokio: if the reader has no progress
                        // but our buffer still has unwritten data, drain the
                        // writer instead of parking. If the buffer is empty,
                        // flush a buffered writer to avoid deadlock when the
                        // reader depends on writer progress (mux echo).
                        if self.pos == self.cap {
                            if self.need_flush {
                                ready!(writer.as_mut().poll_flush(cx))?;
                                self.need_flush = false;
                            }
                            // Buffer is empty and reader has no data — park
                            // until the reader or the idle-check Sleep wakes
                            // us. Returning Pending is critical: `continue`
                            // here would busy-loop and starve the runtime.
                            return Poll::Pending;
                        }
                        // Buffer still has unwritten data — fall through to
                        // the write phase below to make progress.
                    }
                }
            }

            // Write phase: drain whatever is in the buffer. `poll_write_buf`
            // also tops up the reader (marking active) if the writer parks.
            while self.pos < self.cap {
                match self.poll_write_buf(cx, reader.as_mut(), writer.as_mut(), state) {
                    Poll::Ready(Ok(0)) => {
                        state.abort.store(true, Relaxed);
                        metrics::counter!("mux.stream.close.write_error").increment(1);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "write zero byte into writer",
                        )
                        .into()));
                    }
                    Poll::Ready(Ok(i)) => {
                        self.pos += i;
                        self.need_flush = true;
                        // A successful write is data progress — the peer
                        // direction is draining the buffer even though the
                        // reader is idle (buffer is full or EOF). Without
                        // this, Docker pulls of large images timeout when
                        // the 32KB buffer fills and write-back is slow
                        // (e.g. due to mux flow-control backpressure).
                        state.mark_active();
                    }
                    Poll::Ready(Err(e)) => {
                        state.abort.store(true, Relaxed);
                        metrics::counter!("mux.stream.close.write_error").increment(1);
                        return Poll::Ready(Err(e.into()));
                    }
                    Poll::Pending => {
                        // Writer blocked on flow-control backpressure
                        // (e.g. mux send window exhausted). This is NOT
                        // idleness — data is in flight, just throttled.
                        // Without this, Docker pulls of large images hit
                        // idle timeout when the 256KB mux window is full
                        // and WINDOW_UPDATE takes >30s on slow links.
                        state.mark_active();
                        return Poll::Pending;
                    }
                }
            }

            // Buffer drained — reset for the next fill.
            self.pos = 0;
            self.cap = 0;

            // If reader hit EOF, flush and finish.
            if self.read_done {
                ready!(writer.as_mut().poll_flush(cx))?;
                metrics::counter!("mux.stream.close.eof").increment(1);
                return Poll::Ready(Ok(()));
            }
        }
    }
}

/// Future driving `TransferBuffer::poll_copy` to completion.
struct Transfer<'a, R, W>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    reader: &'a mut R,
    writer: &'a mut W,
    buf: TransferBuffer,
    timeout_sec: u64,
    state: Arc<TransferState>,
}

impl<R, W> std::future::Future for Transfer<'_, R, W>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = &mut *self;
        me.buf.poll_copy(
            cx,
            Pin::new(&mut *me.reader),
            Pin::new(&mut *me.writer),
            me.timeout_sec,
            &me.state,
        )
    }
}

async fn timeout_copy<R: AsyncRead + Unpin + ?Sized, W: AsyncWrite + Unpin + ?Sized>(
    r: &mut R,
    w: &mut W,
    timeout_sec: u64,
    state: Arc<TransferState>,
) -> Result<()> {
    let mut buf = TransferBuffer::new(32 * 1024);
    buf.arm_idle_check();
    let result = Transfer {
        reader: r,
        writer: w,
        buf,
        timeout_sec,
        state,
    }
    .await;
    // Shutdown the write side regardless of outcome so the peer sees EOF.
    let _ = w.shutdown().await;
    result
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
        let state = Arc::new(TransferState::new());
        let client_to_server = timeout_copy(
            &mut self.local_reader,
            &mut self.remote_writer,
            idle_timeout_secs as u64,
            state.clone(),
        );
        let server_to_client = timeout_copy(
            &mut self.remote_reader,
            &mut self.local_writer,
            idle_timeout_secs as u64,
            state.clone(),
        );
        try_join(client_to_server, server_to_client).await?;
        Ok(())
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
                let udp_socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
                udp_socket.connect(&open_event.addr).await?;
                let udp_stream = UdpClientStream::new(udp_socket);
                let (mut remote_receiver, mut remote_sender) = tokio::io::split(udp_stream);
                let mut stream = Stream::new(lr, lw, &mut remote_receiver, &mut remote_sender);
                stream.transfer(idle_timeout_secs).await
            } else {
                let mut remote_stream = timeout(
                    timeout_secs,
                    tokio::net::TcpStream::connect(&open_event.addr),
                )
                .await??;
                let (mut remote_receiver, mut remote_sender) = remote_stream.split();
                let mut stream = Stream::new(lr, lw, &mut remote_receiver, &mut remote_sender);
                stream.transfer(idle_timeout_secs).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncWriteExt};

    /// idle timeout: a stalled reader must cause `timeout_copy` to return an
    /// error after the idle budget is exceeded. We use a 1s idle budget and
    /// a reader that never produces data.
    ///
    /// With `CHECK_TIMEOUT_SECS=1` and `timeout_sec=1`, the budget is
    /// `1 / 1 = 1` idle interval. The first Sleep fire at t=1s clears the
    /// initial `active=true` flag (set in `TransferState::new`); the second
    /// fire at t=2s sees `active=false` and triggers the timeout.
    #[tokio::test]
    async fn idle_timeout_fires_on_stalled_reader() {
        // duplex whose writer side we never touch — reader will Pending forever.
        let (mut a, _b) = duplex(1024);
        let state = Arc::new(TransferState::new());
        let mut sink = tokio::io::sink();

        let start = Instant::now();
        let result = timeout_copy(&mut a, &mut sink, 1, state).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected idle timeout error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("idle timeout"),
            "expected idle timeout message, got: {}",
            err
        );
        // First Sleep fires at ~1s (clears initial active flag), second at
        // ~2s (declares timeout). Allow CI slack.
        assert!(
            elapsed >= Duration::from_millis(1900),
            "returned too early: {:?}",
            elapsed
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "took too long: {:?}",
            elapsed
        );
    }

    /// abort: setting `state.abort` mid-flight causes `timeout_copy` to return
    /// an abort error on its next poll.
    #[tokio::test]
    async fn abort_flag_cancels_transfer() {
        let (mut a, mut b) = duplex(1024);
        let state = Arc::new(TransferState::new());

        // Spawn a slow writer that produces 1 byte every 50ms — keeps the
        // reader from hitting EOF so the abort flag is the only exit path.
        let producer = tokio::spawn(async move {
            for _ in 0..100 {
                b.write_all(&[0u8]).await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            b.shutdown().await.unwrap();
        });

        let state_for_abort = state.clone();
        // After 200ms, set abort.
        let aborter = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            state_for_abort.abort.store(true, Relaxed);
        });

        let mut sink = tokio::io::sink();
        let result = timeout_copy(&mut a, &mut sink, 30, state).await;
        aborter.await.unwrap();
        let _ = producer.await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("abort"), "expected abort error, got: {}", err);
    }

    /// Regression: successful writes must reset the idle timer.
    ///
    /// In the Docker pull scenario the 32KB buffer fills, then drains
    /// slowly through a bandwidth-limited mux stream. Before the fix,
    /// only reads marked the transfer active — so a full buffer with
    /// slow writes would hit the idle timeout even though data was
    /// flowing. The fix calls `mark_active()` on every successful write.
    ///
    /// This test validates the `TransferState` contract: after the idle
    /// check clears the active flag, a subsequent `mark_active()` (from a
    /// successful write) must restore it so the next check sees progress.
    #[test]
    fn write_mark_active_resets_idle_check() {
        let state = TransferState::new();
        // Initial state: active == true.
        assert!(state.check_and_reset_active(), "initial active should be true");
        // After reset: active == false.
        assert!(!state.check_and_reset_active(), "second check with no mark should be false");
        // Simulate one write success (the fix adds this in poll_copy).
        state.mark_active();
        // Now active should be true again.
        assert!(state.check_and_reset_active(), "mark_active after write should reset idle");
    }

    /// Without `mark_active()` on writes, two consecutive idle checks
    /// with no read activity in between return `false` on the second
    /// check → idle timeout. This is the bug: only reads reset the flag.
    #[test]
    fn write_without_mark_active_triggers_idle() {
        let state = TransferState::new();
        assert!(state.check_and_reset_active()); // initial
        // Simulate idle interval with no reads AND no write-mark calls.
        assert!(!state.check_and_reset_active()); // timeout!
    }
}
