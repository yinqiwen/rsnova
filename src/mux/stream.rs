use crate::mux::metrics as mux_metrics;
use crate::utils;
use anyhow::Result;
use bytes::{Bytes, BytesMut};
use futures::SinkExt;
use futures::ready;
use futures::task::AtomicWaker;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::PollSender;

/// Per-stream flow control state shared between dispatcher and MuxStream.
/// The dispatcher credits send_window (on inbound WINDOW_UPDATE) and sets closed.
/// MuxStream deducts send_window (on poll_write) and registers write_waker.
pub struct StreamFlow {
    send_window: AtomicU32,
    closed: AtomicBool,
    write_waker: AtomicWaker,
}

impl StreamFlow {
    pub fn new(initial_window: u32) -> Self {
        Self {
            send_window: AtomicU32::new(initial_window),
            closed: AtomicBool::new(false),
            write_waker: AtomicWaker::new(),
        }
    }

    pub fn available(&self) -> u32 {
        if self.closed.load(Ordering::Acquire) {
            return 0;
        }
        self.send_window.load(Ordering::Acquire)
    }

    pub fn credit(&self, increment: u32) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        self.send_window.fetch_add(increment, Ordering::Release);
        self.write_waker.wake();
    }

    pub fn try_consume(&self, desired: usize) -> usize {
        if self.closed.load(Ordering::Acquire) {
            return 0;
        }
        loop {
            let current = self.send_window.load(Ordering::Acquire);
            if current == 0 {
                return 0;
            }
            let n = desired.min(current as usize);
            match self.send_window.compare_exchange(
                current,
                current - n as u32,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return n,
                Err(_) => continue,
            }
        }
    }

    pub fn register_waker(&self, waker: &Waker) {
        self.write_waker.register(waker);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.write_waker.wake();
    }
}

pub type StreamDataReceiver = mpsc::UnboundedReceiver<Option<Bytes>>;

pub struct NewStreamParams {
    pub stream_id: u32,
    pub sender: mpsc::UnboundedSender<Option<Bytes>>,
    pub receiver: Option<StreamDataReceiver>,
    pub flow: Arc<StreamFlow>,
    pub window_update_sender: mpsc::UnboundedSender<(u32, u32)>,
}

pub enum Control {
    AcceptStream(oneshot::Sender<Result<MuxStream>>),
    NewStream(NewStreamParams),
    StreamData(u32, Bytes, bool),
    StreamShutdown(u32, bool),
    StreamClose(u32, bool),
    WindowUpdateFromPeer(u32, u32),
    Ping(u32),
    Pong(u32),
    Close,
}

pub struct MuxStream {
    conn_id: u32,
    id: u32,
    ev_writer: PollSender<Control>,
    inbound_reader: mpsc::UnboundedReceiver<Option<Bytes>>,
    recv_buf: Bytes,
    initial_close: bool,
    close_by_remote: bool,
    read_eof: bool,
    flow: Arc<StreamFlow>,
    consumed_since_update: u32,
    window_update_threshold: u32,
    window_update_sender: mpsc::UnboundedSender<(u32, u32)>,
}

impl MuxStream {
    pub fn new(
        conn_id: u32,
        id: u32,
        ev_writer: mpsc::Sender<Control>,
        inbound_reader: mpsc::UnboundedReceiver<Option<Bytes>>,
        flow: Arc<StreamFlow>,
        initial_stream_window: u32,
        window_update_sender: mpsc::UnboundedSender<(u32, u32)>,
    ) -> Self {
        Self {
            conn_id,
            id,
            ev_writer: PollSender::new(ev_writer),
            inbound_reader,
            recv_buf: Bytes::new(),
            initial_close: false,
            close_by_remote: false,
            read_eof: false,
            flow,
            consumed_since_update: 0,
            window_update_threshold: initial_stream_window / 2,
            window_update_sender,
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    fn close_reader(&mut self) {
        self.inbound_reader.close();
    }

    fn maybe_send_window_update(&mut self) {
        if self.consumed_since_update >= self.window_update_threshold {
            let _ = self
                .window_update_sender
                .send((self.id, self.consumed_since_update));
            self.consumed_since_update = 0;
        }
    }

    fn flush_window_update(&mut self) {
        if self.consumed_since_update > 0 {
            let _ = self
                .window_update_sender
                .send((self.id, self.consumed_since_update));
            self.consumed_since_update = 0;
        }
    }

    /// Zero-copy write path for callers that already hold a `Bytes`.
///
/// The standard `AsyncWrite::poll_write` takes `&[u8]`, forcing an internal
/// `extend_from_slice` copy into an owned `BytesMut` before the bytes can
/// travel across the dispatcher's mpsc channel. When a caller (e.g. the
/// relay loop in `tunnel::stream`) has read data into a `BytesMut` and
/// `split_to`/`freeze`d it into a `Bytes`, this method skips that copy
/// entirely — the `Bytes` is moved straight into the `Control::StreamData`
/// message with only a refcount bump.
///
/// Returns `Poll::Ready(Ok(()))` when the full `data` has been accepted
/// (subject to flow-control window). If the window can't cover all of
/// `data.len()`, returns `Pending` after registering the waker; the caller
/// should retry the same `data` when woken. If `data.len()` exceeds the
/// initial stream window the caller must split it first.
pub fn poll_write_bytes(
    &mut self,
    cx: &mut Context<'_>,
    data: Bytes,
) -> Poll<Result<(), std::io::Error>> {
    if self.close_by_remote || self.flow.is_closed() {
        return Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "stream closed",
        )));
    }
    let len = data.len();
    if len == 0 {
        return Poll::Ready(Ok(()));
    }

    if self.flow.available() < len as u32 {
        self.flow.register_waker(cx.waker());
        if self.flow.is_closed() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stream closed",
            )));
        }
        if self.flow.available() < len as u32 {
            mux_metrics::inc_write_window_wait(self.conn_id);
            return Poll::Pending;
        }
    }

    match self.ev_writer.poll_reserve(cx) {
        Poll::Pending => {
            mux_metrics::inc_poll_reserve_wait(self.conn_id);
            self.flow.register_waker(cx.waker());
            if self.flow.is_closed() {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "stream closed",
                )));
            }
            return Poll::Pending;
        }
        Poll::Ready(Err(e)) => {
            return Poll::Ready(Err(utils::make_io_error(&e.to_string())));
        }
        Poll::Ready(Ok(_)) => {}
    }

    let consumed = self.flow.try_consume(len);
    if consumed < len {
        // Window shrank between the check above and try_consume. Abort the
        // reservation and let the caller retry — partial writes via this
        // path would require the caller to split the Bytes, which loses the
        // zero-copy benefit.
        if self.ev_writer.abort_send() {
            mux_metrics::inc_poll_reserve_aborted(self.conn_id);
        }
        self.flow.register_waker(cx.waker());
        return Poll::Pending;
    }

    let stream_id = self.id;
    match self
        .ev_writer
        .send_item(Control::StreamData(stream_id, data, false))
    {
        Ok(()) => Poll::Ready(Ok(())),
        Err(ex) => Poll::Ready(Err(utils::make_io_error(&ex.to_string()))),
    }
}
}

impl AsyncRead for MuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.recv_buf.is_empty() {
            let copy_n = self.recv_buf.len().min(buf.remaining());
            buf.put_slice(&self.recv_buf[..copy_n]);
            self.recv_buf = if copy_n == self.recv_buf.len() {
                Bytes::new()
            } else {
                self.recv_buf.slice(copy_n..)
            };
            self.consumed_since_update += copy_n as u32;
            self.maybe_send_window_update();
            return Poll::Ready(Ok(()));
        }
        if self.read_eof {
            self.close_reader();
            return Poll::Ready(Ok(()));
        }

        match self.inbound_reader.poll_recv(cx) {
            Poll::Ready(Some(data)) => match data {
                Some(b) => {
                    let mut copy_n = b.len();
                    if copy_n == 0 {
                        self.read_eof = true;
                        self.close_reader();
                        return Poll::Ready(Ok(()));
                    }
                    if copy_n > buf.remaining() {
                        copy_n = buf.remaining();
                    }
                    buf.put_slice(&b[..copy_n]);
                    if copy_n < b.len() {
                        self.recv_buf = b.slice(copy_n..);
                    }
                    self.consumed_since_update += copy_n as u32;
                    self.maybe_send_window_update();
                    Poll::Ready(Ok(()))
                }
                None => {
                    self.close_by_remote = true;
                    self.close_reader();
                    Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "close by remote",
                    )))
                }
            },
            Poll::Ready(None) => {
                self.close_reader();
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "close by remote",
                )))
            }
            Poll::Pending => {
                if self.read_eof {
                    self.close_reader();
                    return Poll::Ready(Ok(()));
                }
                if self.close_by_remote {
                    self.close_reader();
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "close by remote",
                    )));
                }
                Poll::Pending
            }
        }
    }
}

impl AsyncWrite for MuxStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        if self.close_by_remote || self.flow.is_closed() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stream closed",
            )));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if self.flow.available() == 0 {
            self.flow.register_waker(cx.waker());
            if self.flow.is_closed() {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "stream closed",
                )));
            }
            if self.flow.available() == 0 {
                mux_metrics::inc_write_window_wait(self.conn_id);
                return Poll::Pending;
            }
        }

        match self.ev_writer.poll_reserve(cx) {
            Poll::Pending => {
                mux_metrics::inc_poll_reserve_wait(self.conn_id);
                self.flow.register_waker(cx.waker());
                if self.flow.is_closed() {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "stream closed",
                    )));
                }
                return Poll::Pending;
            }
            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(utils::make_io_error(&e.to_string())));
            }
            Poll::Ready(Ok(_)) => {}
        }

        let allowed = self.flow.try_consume(buf.len());
        if allowed == 0 {
            if self.ev_writer.abort_send() {
                mux_metrics::inc_poll_reserve_aborted(self.conn_id);
            }
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stream closed",
            )));
        }

        // Avoid `Bytes::copy_from_slice`, which does a memset + memcpy.
        // `BytesMut::with_capacity` then `extend_from_slice` only does the
        // memcpy, skipping the zero-fill. `freeze()` is a zero-cost ownership
        // transfer into `Bytes`.
        let mut data = BytesMut::with_capacity(allowed);
        data.extend_from_slice(&buf[..allowed]);
        let data = data.freeze();
        let stream_id = self.id;
        match self
            .ev_writer
            .send_item(Control::StreamData(stream_id, data, false))
        {
            Ok(()) => Poll::Ready(Ok(allowed)),
            Err(ex) => Poll::Ready(Err(utils::make_io_error(&ex.to_string()))),
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match ready!(self.ev_writer.poll_flush_unpin(cx)) {
            Err(e) => Poll::Ready(Err(utils::make_io_error(&e.to_string()))),
            Ok(_v) => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        if !self.initial_close {
            self.initial_close = true;
            self.flush_window_update();
            let ctrl = Control::StreamShutdown(self.id, false);
            match ready!(self.ev_writer.poll_reserve(cx)) {
                Err(e) => Poll::Ready(Err(utils::make_io_error(&e.to_string()))),
                Ok(_) => match self.ev_writer.send_item(ctrl) {
                    Ok(()) => Poll::Ready(Ok(())),
                    Err(ex) => Poll::Ready(Err(utils::make_io_error(&ex.to_string()))),
                },
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

impl Drop for MuxStream {
    fn drop(&mut self) {
        self.flush_window_update();
        if let Some(sender) = self.ev_writer.get_ref()
            && !self.close_by_remote
        {
            let stream_close = Control::StreamClose(self.id, false);
            if let Err(e) = sender.try_send(stream_close) {
                mux_metrics::inc_stream_close_drop_failed(self.conn_id);
                tracing::debug!("stream {} drop send close failed: {}", self.id, e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Wake, Waker};

    struct TestWaker {
        woken: std::sync::atomic::AtomicBool,
    }
    impl TestWaker {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                woken: std::sync::atomic::AtomicBool::new(false),
            })
        }
        fn was_woken(&self) -> bool {
            self.woken.load(std::sync::atomic::Ordering::Acquire)
        }
    }
    impl Wake for TestWaker {
        fn wake(self: Arc<Self>) {
            self.woken.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    #[test]
    fn stream_flow_initial_available() {
        let flow = StreamFlow::new(256 * 1024);
        assert_eq!(flow.available(), 256 * 1024);
        assert!(!flow.is_closed());
    }

    #[test]
    fn stream_flow_try_consume_basic() {
        let flow = StreamFlow::new(1000);
        let consumed = flow.try_consume(600);
        assert_eq!(consumed, 600);
        assert_eq!(flow.available(), 400);

        let consumed2 = flow.try_consume(500);
        assert_eq!(consumed2, 400);
        assert_eq!(flow.available(), 0);
    }

    #[test]
    fn stream_flow_try_consume_returns_zero_when_empty() {
        let flow = StreamFlow::new(100);
        let _ = flow.try_consume(100);
        assert_eq!(flow.try_consume(1), 0);
    }

    #[test]
    fn stream_flow_credit_adds_window() {
        let flow = StreamFlow::new(100);
        let _ = flow.try_consume(100);
        assert_eq!(flow.available(), 0);
        flow.credit(50);
        assert_eq!(flow.available(), 50);
    }

    #[test]
    fn stream_flow_credit_wakes_writer() {
        let flow = StreamFlow::new(0);
        let test_waker = TestWaker::new();
        let waker = Waker::from(test_waker.clone());
        flow.register_waker(&waker);
        assert!(!test_waker.was_woken());

        flow.credit(100);
        assert!(test_waker.was_woken());
    }

    #[test]
    fn stream_flow_close_returns_zero_available() {
        let flow = StreamFlow::new(1000);
        flow.close();
        assert_eq!(flow.available(), 0);
        assert!(flow.is_closed());
    }

    #[test]
    fn stream_flow_close_wakes_writer() {
        let flow = StreamFlow::new(0);
        let test_waker = TestWaker::new();
        let waker = Waker::from(test_waker.clone());
        flow.register_waker(&waker);

        flow.close();
        assert!(test_waker.was_woken());
    }

    #[test]
    fn stream_flow_try_consume_returns_zero_when_closed() {
        let flow = StreamFlow::new(1000);
        flow.close();
        assert_eq!(flow.try_consume(100), 0);
    }

    #[test]
    fn stream_flow_credit_noop_when_closed() {
        let flow = StreamFlow::new(0);
        flow.close();
        flow.credit(100);
        assert_eq!(flow.available(), 0);
    }
}
