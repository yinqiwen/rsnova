use crate::utils;
use anyhow::Result;
use bytes::Buf;
use bytes::BytesMut;
use futures::ready;
use futures::SinkExt;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::PollSender;

pub struct MuxStream {
    id: u32,
    ev_writer: PollSender<Control>,
    inbound_reader: mpsc::Receiver<Option<Vec<u8>>>,
    recv_buf: BytesMut,
    initial_close: bool,
    close_by_remote: bool,
    read_eof: bool,
}

type StreamDataReceiver = mpsc::Receiver<Option<Vec<u8>>>;

pub enum Control {
    AcceptStream(oneshot::Sender<Result<MuxStream>>),
    NewStream(
        (
            u32,
            mpsc::Sender<Option<Vec<u8>>>,
            Option<StreamDataReceiver>,
        ),
    ),
    StreamData(u32, Vec<u8>, bool),
    StreamShutdown(u32, bool),
    StreamClose(u32, bool),
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
            initial_close: false,
            close_by_remote: false,
            read_eof: false,
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
            return Poll::Ready(Ok(()));
        };
        if self.read_eof {
            return Poll::Ready(Ok(()));
        }

        match self.inbound_reader.poll_recv(cx) {
            Poll::Ready(Some(data)) => match data {
                Some(b) => {
                    let mut copy_n: usize = b.len();
                    if 0 == copy_n {
                        self.read_eof = true;
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
                    self.close_by_remote = true;
                    Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "close by remote",
                    )))
                }
            },
            Poll::Ready(None) => {
                // Poll::Ready(Ok(()))
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "close by remote",
                )))
            }
            Poll::Pending => {
                if self.read_eof {
                    return Poll::Ready(Ok(()));
                }
                if self.close_by_remote {
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
        if self.close_by_remote {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "close by remote",
            )));
        }
        let ctrl = Control::StreamData(self.id, Vec::from(buf), false);
        match ready!(self.ev_writer.poll_reserve(cx)) {
            Err(e) => Poll::Ready(Err(utils::make_io_error(&e.to_string()))),
            Ok(_v) => match self.ev_writer.send_item(ctrl) {
                Ok(()) => Poll::Ready(Ok(buf.len())),
                Err(ex) => Poll::Ready(Err(utils::make_io_error(&ex.to_string()))),
            },
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
        //Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        if self.read_eof {
            // do nothing
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
        tracing::info!("Stream:{} drop!", self.id);
        if let Some(sender) = self.ev_writer.get_ref() {
            if !self.close_by_remote {
                let ctrl_sender = sender.clone();
                let stream_drop = Control::StreamClose(self.id, false);
                tokio::spawn(async move {
                    let _ = ctrl_sender.send(stream_drop).await;
                });
            }
        }
    }
}
