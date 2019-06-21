use crate::common::io::*;
use bytes::{BufMut, Bytes, BytesMut};
use futures::{Async, Poll, Stream};
use httparse::Status;
use std::collections::vec_deque::VecDeque;
use std::fmt::Write as fmt_write;
use std::io::{Cursor, Error, ErrorKind};
use std::io::{Read, Write};
use tokio::net::TcpStream;
use tokio_io::AsyncRead;
use unicase::Ascii;

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Header {
    /// The name portion of a header.
    ///
    /// A header name must be valid ASCII-US, so it's safe to store as a `&str`.
    pub name: Ascii<String>,
    /// The value portion of a header.
    ///
    /// While headers **should** be ASCII-US, the specification allows for
    /// values that may not be, and so the value is stored as bytes.
    pub value: Bytes,
}
#[derive(Clone, PartialEq, Debug, Default)]
pub struct HttpRequest {
    /// The request method, such as `GET`.
    pub method: Option<String>,
    /// The request path, such as `/about-us`.
    pub path: Option<String>,
    /// The request version, such as `HTTP/1.1`.
    pub version: Option<u8>,
    /// The request headers.
    pub headers: Vec<Header>,
}

impl HttpRequest {
    pub fn remove_header(&mut self, header: &str) {
        self.headers.retain(|x| x.name != Ascii::new(header));
    }

    pub fn get_header(&self, header: &str) -> Option<String> {
        for h in &self.headers {
            if h.name == Ascii::new(header) {
                let v = String::from_utf8_lossy(&h.value[..]);
                return Some(String::from(v));
            }
        }
        None
    }
    pub fn to_bytes(&self) -> Bytes {
        let mut buf = BytesMut::new();
        if let Some(v) = &self.method {
            buf.reserve(v.len() + 1);
            buf.write_str(v.as_str());
            buf.write_char(' ');
        }
        if let Some(v) = &self.path {
            buf.reserve(v.len() + 1);
            buf.write_str(v.as_str());
            buf.write_char(' ');
        }
        if let Some(v) = &self.version {
            buf.reserve(10);
            if *v == 0 {
                buf.write_str("HTTP/1.0");
            } else {
                buf.write_str("HTTP/1.1");
            }
        }
        buf.reserve(2);
        buf.write_str("\r\n");
        for h in &self.headers {
            buf.reserve(h.name.len() + 1 + h.value.len() + 2);
            buf.write_str(h.name.as_str());
            buf.write_char(':');
            buf.reserve(h.value.len() + 2);
            buf.put_slice(&h.value[..]);
            buf.write_str("\r\n");
        }
        buf.reserve(2);
        buf.write_str("\r\n");
        buf.freeze()
    }
}

pub enum HttpMessage {
    Request(HttpRequest),
    Chunk(Bytes),
}

enum HttpDecodeState {
    DECODING_HEADER,
    DECODING_BODY,
}

pub struct HttpProxyReader<T> {
    reader: T,
    state: HttpDecodeState,
    recv_buf: BytesMut,
    ready_buf: Cursor<BytesMut>,
    body_length: i64,
    body_tail: VecDeque<u8>,
}

impl<T: AsyncRead> HttpProxyReader<T> {
    pub fn new(sock: T) -> Self {
        Self {
            reader: sock,
            state: HttpDecodeState::DECODING_HEADER,
            recv_buf: BytesMut::new(),
            ready_buf: Cursor::new(BytesMut::new()),
            body_length: 0,
            body_tail: VecDeque::new(),
        }
    }
    pub fn parse_request(&mut self) -> Result<usize, std::io::Error> {
        self.body_length = 0;
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut req = httparse::Request::new(&mut headers);
        let mut header_len: usize = 0;
        let data = &mut self.recv_buf[..];
        match req.parse(data) {
            Ok(Status::Complete(n)) => {
                self.state = HttpDecodeState::DECODING_BODY;
                self.body_tail.clear();
                header_len = n;
            }
            Ok(Status::Partial) => {
                return Ok(0);
            }
            Err(_) => {
                return Err(Error::from(ErrorKind::Other));
            }
        };
        let mut hreq: HttpRequest = Default::default();
        for h in req.headers {
            let mut header = Header {
                name: Ascii::new(String::from(h.name)),
                value: Bytes::from(h.value),
            };
            let hn = header.name.to_ascii_lowercase();
            match hn.as_str() {
                "proxy-authorization" => {
                    continue;
                }
                "proxy-connection" => {
                    header.name = Ascii::new(String::from("Connection"));
                    continue;
                }
                "transfer-encoding" => {
                    if String::from_utf8_lossy(&header.value[..]).contains("chunked") {
                        self.body_length = -1;
                    }
                }
                "content-length" => {
                    match String::from_utf8_lossy(&header.value[..]).parse::<u64>() {
                        Ok(v) => {
                            self.body_length = v as i64;
                        }
                        Err(_) => {
                            error!(
                                "invalid content length value:{}",
                                String::from_utf8_lossy(&header.value[..])
                            );
                            //return Err(Error::from(ErrorKind::Other));
                        }
                    }
                }
                "host" => {}
                _ => {
                    //do nothing
                }
            }
            hreq.headers.push(header);
        }
        self.recv_buf.advance(header_len);
        return Ok(header_len);
    }

    fn read_local_buf(&mut self, buf: &mut [u8]) -> usize {
        if self.ready_buf.get_ref().len() == 0 {
            return 0;
        }
        return self.ready_buf.read(buf).unwrap();
    }
}

impl<T: AsyncRead> AsyncRead for HttpProxyReader<T> {}

impl<T: AsyncRead> Read for HttpProxyReader<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.read_local_buf(buf);
        if n > 0 {
            return Ok(n);
        }
        let mut not_ready = false;
        loop {
            not_ready = false;
            self.recv_buf.reserve(1024);
            let pos = self.recv_buf.len();
            let cap = self.recv_buf.capacity();
            unsafe {
                self.recv_buf.set_len(cap);
            }
            match self.reader.poll_read(&mut self.recv_buf[pos..]) {
                Ok(Async::Ready(n)) => {
                    info!("http Recv {} bytes.", n);
                    unsafe {
                        self.recv_buf.set_len(pos + n);
                    }
                    if 0 == n {
                        return Err(Error::from(ErrorKind::WouldBlock));
                    }
                }
                Err(e) => {
                    return Err(e);
                }
                Ok(Async::NotReady) => {
                    unsafe {
                        self.recv_buf.set_len(pos);
                    }
                    if self.recv_buf.len() == 0 {
                        return Err(Error::from(ErrorKind::WouldBlock));
                    }
                    not_ready = true;
                }
            }
            match &self.state {
                DECODING_HEADER => {
                    if not_ready {
                        return Err(Error::from(ErrorKind::WouldBlock));
                    }
                    self.parse_request();
                    return Ok(self.read_local_buf(buf));
                }
                DECODING_BODY => {
                    if self.body_length < 0 {
                        let chunk = Bytes::from(&self.recv_buf[..]);
                        self.recv_buf.clear();
                        let mut tail_pos = 0;
                        if chunk.len() > 5 {
                            tail_pos = chunk.len() - 5;
                            self.body_tail.clear();
                        }
                        while tail_pos < chunk.len() {
                            self.body_tail.push_back(chunk[tail_pos]);
                            tail_pos = tail_pos + 1;
                            if self.body_tail.len() > 5 {
                                self.body_tail.pop_front();
                            }
                        }

                        if self.body_tail.len() == 5 {
                            if self.body_tail[0] == '0' as u8
                                && self.body_tail[1] == '\r' as u8
                                && self.body_tail[2] == '\n' as u8
                                && self.body_tail[3] == '\r' as u8
                                && self.body_tail[4] == '\n' as u8
                            {
                                self.state = HttpDecodeState::DECODING_HEADER;
                            }
                        }
                        return Ok(self.read_local_buf(buf));
                    } else if self.body_length > 0 {
                        self.body_length = self.body_length - self.recv_buf.len() as i64;
                        let chunk = Bytes::from(&self.recv_buf[..]);
                        self.recv_buf.clear();
                        return Ok(self.read_local_buf(buf));
                    } else {
                        self.state = HttpDecodeState::DECODING_HEADER;
                        continue;
                    }
                }
            }
        }
    }
}
