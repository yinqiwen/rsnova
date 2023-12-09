use crate::utils;
use anyhow::{anyhow, Result};
use bytes::Buf;
use bytes::BytesMut;
use futures::ready;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;
use std::task::{Context, Poll};
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::PollSender;

struct MuxStreamState {
    close_by_remote: AtomicBool,
    io_active_timestamp_secs: AtomicU64,
}

impl MuxStreamState {
    fn new() -> Self {
        Self {
            close_by_remote: AtomicBool::new(false),
            io_active_timestamp_secs: AtomicU64::new(0),
        }
    }
}

pub struct MuxStream {
    id: u32,
    ev_writer: PollSender<Control>,
    inbound_reader: mpsc::Receiver<Option<Vec<u8>>>,
    recv_buf: BytesMut,
    state: MuxStreamState,
    initial_close: bool,
}

pub enum Control {
    AcceptStream(oneshot::Sender<Result<MuxStream>>),
    NewStream(
        (
            u32,
            mpsc::Sender<Option<Vec<u8>>>,
            Option<mpsc::Receiver<Option<Vec<u8>>>>,
        ),
    ),
    StreamData(u32, Vec<u8>, bool),
    StreamShutdown(u32, bool),
    StreamClose(u32),
    Ping,
    Close,
}

fn fill_read_buf(src: &mut BytesMut, dst: &mut ReadBuf<'_>) -> usize {
    if src.is_empty() {
        return 0;
    }
    let mut n = src.len();
    if n > dst.remaining() {
        n = dst.remaining();
    }

    dst.put_slice(&src[0..n]);
    src.advance(n);
    if src.is_empty() {
        src.clear();
    }
    n
}

impl MuxStream {
    pub fn new(
        id: u32,
        ev_writer: mpsc::Sender<Control>,
        inbound_reader: mpsc::Receiver<Option<Vec<u8>>>,
    ) -> Self {
        Self {
            id,
            ev_writer: PollSender::new(ev_writer),
            inbound_reader,
            recv_buf: BytesMut::new(),
            state: MuxStreamState::new(),
            initial_close: false,
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }
}

impl AsyncRead for MuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.recv_buf.is_empty() {
            fill_read_buf(&mut self.recv_buf, buf);
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
        };
        match self.inbound_reader.poll_recv(cx) {
            Poll::Ready(Some(data)) => match data {
                Some(b) => {
                    let mut copy_n: usize = b.len();
                    if 0 == copy_n {
                        return Poll::Ready(Ok(()));
                    }
                    if copy_n > buf.remaining() {
                        copy_n = buf.remaining();
                    }
                    buf.put_slice(&b[0..copy_n]);
                    if copy_n < b.len() {
                        self.recv_buf.extend_from_slice(&b[copy_n..]);
                    }
                    Poll::Ready(Ok(()))
                }
                None => {
                    self.state
                        .close_by_remote
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "close by remote",
                    )))
                }
            },
            Poll::Ready(None) => {
                // self.eof_close = true;
                //error!("[{}]####3 Close", state.stream_id);
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for MuxStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let ctrl = Control::StreamData(self.id, Vec::from(buf), false);
        match ready!(self.ev_writer.poll_reserve(cx)) {
            Err(e) => Poll::Ready(Err(utils::make_io_error(&e.to_string()))),
            Ok(v) => match self.ev_writer.send_item(ctrl) {
                Ok(()) => Poll::Ready(Ok(buf.len())),
                Err(ex) => Poll::Ready(Err(utils::make_io_error(&ex.to_string()))),
            },
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        if !self.initial_close {
            self.initial_close = true;
            let ctrl = Control::StreamShutdown(self.id, false);
            match ready!(self.ev_writer.poll_reserve(cx)) {
                Err(e) => Poll::Ready(Err(utils::make_io_error(&e.to_string()))),
                Ok(v) => match self.ev_writer.send_item(ctrl) {
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
        match self.ev_writer.get_ref() {
            Some(sender) => {
                if !self.initial_close {
                    let ctrl_sender = sender.clone();
                    let stream_close = Control::StreamShutdown(self.id, false);
                    tokio::spawn(async move {
                        let _ = ctrl_sender.send(stream_close).await;
                    });
                }
                if !self.state.close_by_remote.load(SeqCst) {
                    let ctrl_sender = sender.clone();
                    let stream_drop = Control::StreamClose(self.id);
                    tokio::spawn(async move {
                        let _ = ctrl_sender.send(stream_drop).await;
                    });
                }
            }
            None => {}
        }
    }
}
