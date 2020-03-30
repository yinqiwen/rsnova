use bytes::Bytes;
use bytes::BytesMut;
use std::error::Error;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::TcpStream;

pub fn make_error(desc: &str) -> Box<dyn Error> {
    Box::new(std::io::Error::new(std::io::ErrorKind::Other, desc))
}

pub fn make_io_error(desc: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, desc)
}

pub struct RelayState {
    shutdown: bool,
    waker: Option<Waker>,
}

impl RelayState {
    pub fn new() -> Self {
        Self {
            shutdown: false,
            waker: None,
        }
    }
    pub fn is_closed(&self) -> bool {
        self.shutdown
    }
    pub fn close(&mut self) {
        if !self.shutdown {
            self.shutdown = true;
            if let Some(waker) = self.waker.take() {
                waker.wake()
            }
        }
    }
}

pub struct RelayBufCopy<'a, R: ?Sized, W: ?Sized> {
    reader: &'a mut R,
    read_done: bool,
    writer: &'a mut W,
    pos: usize,
    cap: usize,
    amt: u64,
    buf: Vec<u8>,
    state: Arc<Mutex<RelayState>>,
}

pub fn relay_buf_copy<'a, R, W>(
    reader: &'a mut R,
    writer: &'a mut W,
    buf: Vec<u8>,
    state: Arc<Mutex<RelayState>>,
) -> RelayBufCopy<'a, R, W>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    RelayBufCopy {
        reader,
        read_done: false,
        writer,
        amt: 0,
        pos: 0,
        cap: 0,
        buf,
        state,
    }
}

impl<R, W> Future for RelayBufCopy<'_, R, W>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    type Output = io::Result<u64>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        loop {
            if self.state.lock().unwrap().shutdown {
                return Poll::Ready(Ok(self.amt));
            }
            // If our buffer is empty, then we need to read some data to
            // continue.
            if self.pos == self.cap && !self.read_done {
                let me = &mut *self;
                let pin_reader = Pin::new(&mut *me.reader);
                let n = match pin_reader.poll_read(cx, &mut me.buf) {
                    Poll::Pending => {
                        let mut shared_state = self.state.lock().unwrap();
                        shared_state.waker = Some(cx.waker().clone());
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(e)) => {
                        return Poll::Ready(Err(e));
                    }
                    Poll::Ready(Ok(n)) => n,
                };
                //let n = ready!(Pin::new(&mut *me.reader).poll_read(cx, &mut me.buf))?;
                if n == 0 {
                    self.read_done = true;
                } else {
                    self.pos = 0;
                    self.cap = n;
                }
            }

            // If our buffer has some data, let's write it out!
            while self.pos < self.cap {
                let me = &mut *self;
                let pin_writer = Pin::new(&mut *me.writer);
                let i = match pin_writer.poll_write(cx, &me.buf[me.pos..me.cap]) {
                    Poll::Pending => {
                        let mut shared_state = self.state.lock().unwrap();
                        shared_state.waker = Some(cx.waker().clone());
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(e)) => {
                        return Poll::Ready(Err(e));
                    }
                    Poll::Ready(Ok(n)) => n,
                };
                //let i = ready!(Pin::new(&mut *me.writer).poll_write(cx, &me.buf[me.pos..me.cap]))?;
                if i == 0 {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write zero byte into writer",
                    )));
                } else {
                    self.pos += i;
                    self.amt += i as u64;
                }
            }

            // If we've written all the data and we've seen EOF, flush out the
            // data and finish the transfer.
            if self.pos == self.cap && self.read_done {
                let me = &mut *self;
                let pin_writer = Pin::new(&mut *me.writer);
                match pin_writer.poll_flush(cx) {
                    Poll::Pending => {
                        let mut shared_state = self.state.lock().unwrap();
                        shared_state.waker = Some(cx.waker().clone());
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(e)) => {
                        return Poll::Ready(Err(e));
                    }
                    Poll::Ready(_) => {}
                }
                //ready!(Pin::new(&mut *me.writer).poll_flush(cx))?;
                return Poll::Ready(Ok(self.amt));
            }
        }
    }
}

pub async fn read_until_separator(
    stream: &mut TcpStream,
    separator: &str,
) -> Result<(Bytes, Bytes), std::io::Error> {
    let mut buf = BytesMut::with_capacity(1024);
    let mut b = [0u8; 1024];
    loop {
        let n = stream.read(&mut b).await?;
        if n > 0 {
            buf.extend_from_slice(&b[0..n]);
        } else {
            return Ok((buf.freeze(), Bytes::default()));
        }
        if let Some(pos) = twoway::find_bytes(&buf[..], separator.as_bytes()) {
            let wsize = separator.len();
            let body = buf.split_off(pos + wsize);
            return Ok((buf.freeze(), body.freeze()));
        }
    }
}
