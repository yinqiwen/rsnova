use crate::common::utils::*;

use std::io::ErrorKind;
use tokio::prelude::*;
use tokio_io::AsyncRead;

use bytes::Buf;
use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;

use std::io;
use std::mem;

use futures::{Future, Poll};

use std::net::SocketAddr;

pub enum HostPort {
    DomainPort(String, u16),
    IPPort(SocketAddr),
}

#[derive(Debug)]
enum State<A> {
    Reading {
        separator: String,
        buf: BytesMut,
        reader: A,
    },
    Empty,
}

pub struct ReadUntilSeparator<A> {
    state: State<A>,
}

impl<A> Future for ReadUntilSeparator<A>
where
    A: AsyncRead,
{
    type Item = (A, Bytes, Bytes);
    type Error = std::io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut r = [0u8; 1024];
        loop {
            match self.state {
                State::Reading {
                    ref mut separator,
                    ref mut buf,
                    ref mut reader,
                } => match reader.poll_read(&mut r) {
                    Ok(Async::NotReady) => {
                        return Ok(Async::NotReady);
                    }
                    Ok(Async::Ready(n)) => {
                        buf.reserve(n);
                        buf.put_slice(&r[0..n]);
                        if let Some(pos) = find_str_in_bytes(&buf, separator.as_str()) {
                            let wsize = separator.len();
                            let body = buf.split_off(pos + wsize);
                            match mem::replace(&mut self.state, State::Empty) {
                                State::Reading {
                                    separator: _,
                                    buf,
                                    reader,
                                } => return Ok((reader, buf.freeze(), body.freeze()).into()),
                                State::Empty => unreachable!(),
                            }
                        }
                        continue;
                    }
                    Err(e) => {
                        return Err(e);
                    }
                },
                State::Empty => panic!("poll ReadUntil after it's done"),
            }
        }
    }
}

pub fn read_until_separator<A>(a: A, separator: &str, buf: &[u8]) -> ReadUntilSeparator<A>
where
    A: AsyncRead,
{
    ReadUntilSeparator {
        state: State::Reading {
            separator: String::from(separator),
            buf: BytesMut::from(buf),
            reader: a,
        },
    }
}

pub struct PeekExact<R: AsyncRead, T> {
    state: PeekState<R, T>,
}

enum PeekState<R: AsyncRead, T> {
    Reading {
        a: PeekableReader<R>,
        buf: T,
        pos: usize,
    },
    Empty,
}

/// Creates a future which will read exactly enough bytes to fill `buf`,
/// returning an error if EOF is hit sooner.
///
/// The returned future will resolve to both the I/O stream as well as the
/// buffer once the read operation is completed.
///
/// In the case of an error the buffer and the object will be discarded, with
/// the error yielded. In the case of success the object will be destroyed and
/// the buffer will be returned, with all data read from the stream appended to
/// the buffer.
pub fn peek_exact<R, T>(a: R, buf: T) -> PeekExact<R, T>
where
    R: AsyncRead,
    T: AsMut<[u8]>,
{
    PeekExact {
        state: PeekState::Reading {
            a: PeekableReader::new(a),
            buf: buf,
            pos: 0,
        },
    }
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "early eof")
}

impl<R, T> Future for PeekExact<R, T>
where
    R: AsyncRead,
    T: AsMut<[u8]>,
{
    type Item = (PeekableReader<R>, T);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, io::Error> {
        match self.state {
            PeekState::Reading {
                ref mut a,
                ref mut buf,
                ref mut pos,
            } => {
                let buf = buf.as_mut();
                while *pos < buf.len() {
                    let n = try_ready!(a.poll_peek(&mut buf[*pos..]));
                    *pos += n;
                    if n == 0 {
                        return Err(eof());
                    }
                }
            }
            PeekState::Empty => panic!("poll a ReadExact after it's done"),
        }

        match mem::replace(&mut self.state, PeekState::Empty) {
            PeekState::Reading { a, buf, .. } => Ok((a, buf).into()),
            PeekState::Empty => panic!(),
        }
    }
}

pub struct PeekableReader<T> {
    inner: T,
    peek_buf: BytesMut,
}

impl<T> PeekableReader<T>
where
    T: AsyncRead,
{
    pub fn new(c: T) -> Self {
        Self {
            inner: c,
            peek_buf: BytesMut::new(),
        }
    }
    pub fn poll_peek(&mut self, buf: &mut [u8]) -> Poll<usize, std::io::Error> {
        loop {
            let cur_n = self.peek_buf.len();
            if cur_n < buf.len() {
                self.peek_buf.reserve(buf.len() - cur_n);
                unsafe {
                    self.peek_buf.set_len(buf.len());
                }
                match self.inner.poll_read(&mut self.peek_buf[cur_n..]) {
                    Err(e) => {
                        unsafe {
                            self.peek_buf.set_len(cur_n);
                        }
                        return Err(e);
                    }
                    Ok(Async::NotReady) => {
                        unsafe {
                            self.peek_buf.set_len(cur_n);
                        }
                        return Ok(Async::NotReady);
                    }
                    Ok(Async::Ready(n)) => unsafe {
                        self.peek_buf.set_len(cur_n + n);
                    },
                }
            } else {
                buf.copy_from_slice(&self.peek_buf[0..buf.len()]);
                return Ok(Async::Ready(buf.len()));
            }
        }
    }
}

impl<T> AsyncRead for PeekableReader<T> where T: AsyncRead {}

impl<T> Read for PeekableReader<T>
where
    T: AsyncRead,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.peek_buf.len() == 0 {
            return self.inner.read(buf);
        }
        let mut n = self.peek_buf.len();
        if n > buf.len() {
            n = buf.len();
        }
        buf.copy_from_slice(&self.peek_buf[0..n]);
        self.peek_buf.advance(n);
        Ok(n)
    }
}
