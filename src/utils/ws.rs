use crate::utils::{fill_read_buf, make_io_error};
use bytes::BytesMut;
use futures::stream::{SplitSink, SplitStream};
use futures::{Future, Sink, SinkExt, Stream, StreamExt};
use std::error::Error;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tungstenite::protocol::Message;

pub struct WebsocketReader<S> {
    stream: SplitStream<WebSocketStream<S>>,
    recv_buf: BytesMut,
}

impl<S> AsyncRead for WebsocketReader<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let Self { stream, recv_buf } = &mut *self;
        if !recv_buf.is_empty() {
            let n = fill_read_buf(recv_buf, buf);
            return Poll::Ready(Ok(n));
        }
        recv_buf.clear();
        pin_mut!(stream);
        match stream.poll_next(cx) {
            Poll::Ready(Some(msg)) => {
                let m = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        return Poll::Ready(Err(make_io_error(e.description())));
                    }
                };
                if !m.is_binary() {
                    return Poll::Ready(Err(make_io_error("invalid msg type")));
                }
                let data = m.into_data();
                let mut copy_n = data.len();
                if 0 == copy_n {
                    //close
                    return Poll::Ready(Ok(0));
                }
                if copy_n > buf.len() {
                    copy_n = buf.len();
                }
                // //info!("[{}]tx ready {} ", state.stream_id, copy_n);
                buf[0..copy_n].copy_from_slice(&data[0..copy_n]);
                if copy_n < data.len() {
                    recv_buf.extend_from_slice(&data[copy_n..]);
                }
                Poll::Ready(Ok(copy_n))
            }
            Poll::Ready(None) => {
                //state.close();
                //rx.close();
                Poll::Ready(Ok(0))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> WebsocketReader<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(stream: SplitStream<WebSocketStream<S>>) -> Self {
        Self {
            stream,
            recv_buf: BytesMut::new(),
        }
    }
}

pub struct WebsocketWriter<S> {
    sink: SplitSink<WebSocketStream<S>, Message>,
}

impl<S> AsyncWrite for WebsocketWriter<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let Self { sink } = &mut *self;
        let blen = buf.len();
        let msg = Message::binary(buf);
        let future = sink.send(msg);
        pin_mut!(future);
        match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(make_io_error(e.description()))),
            Poll::Ready(Ok(())) => Poll::Ready(Ok(blen)),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let Self { sink } = &mut *self;
        let future = sink.close();
        pin_mut!(future);
        match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(make_io_error(e.description()))),
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
        }
    }
}

impl<S> WebsocketWriter<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(sink: SplitSink<WebSocketStream<S>, Message>) -> Self {
        Self { sink }
    }
}
