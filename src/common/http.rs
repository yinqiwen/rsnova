use crate::common::io::*;
use bytes::{Bytes, BytesMut};
use futures::{Async, Poll, Stream};
use httparse::Status;
use std::io::{Error, ErrorKind};
use tokio::net::TcpStream;
use tokio_io::AsyncRead;

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Header {
    /// The name portion of a header.
    ///
    /// A header name must be valid ASCII-US, so it's safe to store as a `&str`.
    pub name: String,
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

pub enum HttpMessage {
    Request(HttpRequest),
    Chunk(Bytes),
}

enum HttpDecodeState {
    DECODING_HEADER,
    DECODING_BODY,
}

pub struct HttpStream {
    socket: TcpStream,
    state: HttpDecodeState,
    recv_buf: BytesMut,
    body_length: i64,
}

impl HttpStream {
    pub fn new(sock: TcpStream) -> Self {
        Self {
            socket: sock,
            state: HttpDecodeState::DECODING_HEADER,
            recv_buf: BytesMut::new(),
            body_length: 0,
        }
    }
}

impl Stream for HttpStream {
    type Item = HttpMessage;

    /// The type of error this stream may generate.
    type Error = std::io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.recv_buf.reserve(1024);
        let pos = self.recv_buf.len();
        match self.socket.poll_read(&mut self.recv_buf[pos..]) {
            Ok(Async::Ready(n)) => {
                if 0 == n {
                    return Err(Error::from(ErrorKind::ConnectionReset));
                }
                unsafe {
                    self.recv_buf.set_len(pos + n);
                }
            }
            Err(e) => {
                return Err(e);
            }
            Ok(Async::NotReady) => {
                return Ok(Async::NotReady);
            }
        }
        match &self.state {
            DECODING_HEADER => {
                let mut headers = [httparse::EMPTY_HEADER; 32];
                let mut req = httparse::Request::new(&mut headers);
                let mut header_len: usize = 0;
                let data = &mut self.recv_buf[..];
                match req.parse(data) {
                    Ok(Status::Complete(n)) => {
                        self.state = HttpDecodeState::DECODING_BODY;
                        header_len = n;
                    }
                    Ok(Status::Partial) => {
                        return Ok(Async::Ready(None));
                    }
                    Err(e) => {
                        return Err(Error::from(ErrorKind::Other));
                    }
                }
                let mut hreq: HttpRequest = Default::default();
                if let Some(v) = req.method {
                    hreq.method = Some(String::from(v));
                }
                if let Some(v) = req.path {
                    hreq.path = Some(String::from(v));
                }
                if let Some(v) = req.version {
                    hreq.version = Some(v);
                }
                for h in req.headers {
                    let header = Header {
                        name: String::from(h.name),
                        value: Bytes::from(h.value),
                    };
                    let hn = header.name.to_ascii_lowercase();
                    match hn.as_str() {
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
                                    return Err(Error::from(ErrorKind::Other));
                                }
                            }
                        }
                        _ => {
                            //do nothing
                        }
                    }
                    hreq.headers.push(header);
                }
                self.recv_buf.advance(header_len);
                return Ok(Async::Ready(Some(HttpMessage::Request(hreq))));
            }
            DECODING_BODY => {
                if self.body_length < 0 {
                    let chunk = Bytes::from(&self.recv_buf[..]);
                    self.recv_buf.clear();

                    return Ok(Async::Ready(Some(HttpMessage::Chunk(chunk))));
                } else if self.body_length > 0 {
                    self.body_length = self.body_length - self.recv_buf.len() as i64;
                    let chunk = Bytes::from(&self.recv_buf[..]);
                    self.recv_buf.clear();
                    return Ok(Async::Ready(Some(HttpMessage::Chunk(chunk))));
                } else {
                    self.state = HttpDecodeState::DECODING_HEADER;
                    return Ok(Async::Ready(None));
                }
                return Ok(Async::Ready(None));
            }
        }
    }
}
