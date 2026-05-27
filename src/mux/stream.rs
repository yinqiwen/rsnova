use crate::utils;
use anyhow::Result;
use bytes::Bytes;
use futures::ready;
use futures::task::AtomicWaker;
use futures::SinkExt;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
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
}

pub enum Control {
    AcceptStream(oneshot::Sender<Result<MuxStream>>),
    NewStream(NewStreamParams),
    StreamData(u32, Bytes, bool),
    StreamShutdown(u32, bool),
    StreamClose(u32, bool),
    WindowUpdateFromPeer(u32, u32),
    WindowUpdateToPeer(u32, u32),
    Ping(u32),
    Pong(u32),
    Close,
}

pub struct MuxStream {
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
}

impl MuxStream {
    pub fn new(
        id: u32,
        ev_writer: mpsc::Sender<Control>,
        inbound_reader: mpsc::UnboundedReceiver<Option<Bytes>>,
        flow: Arc<StreamFlow>,
        initial_stream_window: u32,
    ) -> Self {
        Self {
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
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    fn close_reader(&mut self) {
        self.inbound_reader.close();
    }

    fn maybe_send_window_update(&mut self, bytes_read: u32) {
        self.consumed_since_update += bytes_read;
        if self.consumed_since_update >= self.window_update_threshold {
            if let Some(sender) = self.ev_writer.get_ref() {
                match sender.try_send(Control::WindowUpdateToPeer(
                    self.id,
                    self.consumed_since_update,
                )) {
                    Ok(()) => {
                        self.consumed_since_update = 0;
                    }
                    Err(_) => {
                        // Keep consumed_since_update — retry on next poll_read
                    }
                }
            }
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
            self.maybe_send_window_update(copy_n as u32);
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
                    self.maybe_send_window_update(copy_n as u32);
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
                return Poll::Pending;
            }
        }

        match self.ev_writer.poll_reserve(cx) {
            Poll::Pending => {
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
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stream closed",
            )));
        }

        let data = Bytes::copy_from_slice(&buf[..allowed]);
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
        if self.read_eof {
            Poll::Ready(Ok(()))
        } else if !self.initial_close {
            self.initial_close = true;
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
        if let Some(sender) = self.ev_writer.get_ref() {
            if !self.close_by_remote {
                let stream_close = Control::StreamClose(self.id, false);
                if let Err(e) = sender.try_send(stream_close) {
                    tracing::debug!("stream {} drop send close failed: {}", self.id, e);
                }
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
