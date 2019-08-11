use bytes::{BufMut, Bytes, BytesMut};
use futures::Async;
use httparse::Status;
use std::collections::vec_deque::VecDeque;
use std::fmt::Write as fmt_write;
use std::io::Read;
use std::io::{Error, ErrorKind};
use tokio::io::AsyncRead;
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
#[derive(Debug, PartialEq)]
enum HttpDecodeState {
    DECODING_HEADER,
    DECODING_BODY,
}

pub struct HttpProxyReader<T> {
    reader: T,
    state: HttpDecodeState,
    recv_buf: BytesMut,
    http_buf: BytesMut,
    body_length: i64,
    body_tail: VecDeque<u8>,
    remote_host: String,
    counter: u32,
}

impl<T: AsyncRead> HttpProxyReader<T> {
    pub fn new(sock: T) -> Self {
        Self {
            reader: sock,
            state: HttpDecodeState::DECODING_HEADER,
            recv_buf: BytesMut::new(),
            http_buf: BytesMut::new(),
            body_length: 0,
            body_tail: VecDeque::new(),
            remote_host: String::new(),
            counter: 0,
        }
    }
    pub fn get_remote_addr(&self) -> &str {
        self.remote_host.as_str()
    }
    pub fn add_recv_content(&mut self, b: &[u8]) {
        self.recv_buf.reserve(b.len());
        self.recv_buf.put_slice(b);
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
                "host" => {
                    self.remote_host = String::from(String::from_utf8_lossy(h.value));
                    if !self.remote_host.contains(':') {
                        self.remote_host.push_str(":80");
                    }
                }
                _ => {
                    //do nothing
                }
            }
            hreq.headers.push(header);
        }
        if let Some(v) = req.path {
            if v.starts_with("http://") {
                let vv = &v[7..];
                if let Some(n) = vv.find('/') {
                    hreq.path = Some(String::from(&vv[n..]));
                } else {
                    hreq.path = Some(String::from(v));
                }
            } else {
                hreq.path = Some(String::from(v));
            }
        }
        if let Some(v) = req.method {
            hreq.method = Some(String::from(v));
        }
        if let Some(v) = req.version {
            hreq.version = Some(v);
        }
        let b = hreq.to_bytes();
        self.http_buf.clear();
        self.http_buf.reserve(b.len());
        self.http_buf.put_slice(&b[..]);
        self.recv_buf.advance(header_len);
        if 0 == self.body_length {
            self.switch_reading_header();
            info!(
                "switch to reading header while data len:{}",
                self.recv_buf.len()
            );
        }

        return Ok(header_len);
    }

    fn read_local_http(&mut self, buf: &mut [u8]) -> usize {
        if self.http_buf.len() == 0 {
            return 0;
        }
        let mut n = buf.len();
        if n > self.http_buf.len() {
            n = self.http_buf.len();
        }
        buf[0..n].copy_from_slice(&self.http_buf[0..n]);
        self.http_buf.advance(n);
        if self.http_buf.len() == 0 {
            self.http_buf.clear();
        }
        return n;
    }

    fn read_recv_buf(&mut self, buf: &mut [u8]) -> usize {
        let mut n = buf.len();
        if n > self.recv_buf.len() {
            n = self.recv_buf.len();
        }
        buf[0..n].copy_from_slice(&self.recv_buf[0..n]);
        self.recv_buf.advance(n);
        if self.recv_buf.len() == 0 {
            self.recv_buf.clear();
        }
        return n;
    }

    fn switch_reading_header(&mut self) {
        self.state = HttpDecodeState::DECODING_HEADER;
        self.body_tail.clear();
        self.body_length = 0;
    }

    fn read_body(&mut self, buf: &mut [u8]) -> usize {
        if self.state == HttpDecodeState::DECODING_HEADER {
            return 0;
        }
        if self.body_length < 0 {
            let mut tail_pos = 0;
            if self.recv_buf.len() > 5 {
                tail_pos = self.recv_buf.len() - 5;
                self.body_tail.clear();
            }
            while tail_pos < self.recv_buf.len() {
                self.body_tail.push_back(self.recv_buf[tail_pos]);
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
                    self.switch_reading_header();
                }
            }
            return self.read_recv_buf(buf);
        } else if self.body_length > 0 {
            let n = self.read_recv_buf(buf);
            self.body_length = self.body_length - n as i64;
            if self.body_length == 0 {
                self.switch_reading_header();
            }
            return n;
        } else {
            self.switch_reading_header();
            return 0;
        }
    }
}

impl<T: AsyncRead> AsyncRead for HttpProxyReader<T> {}

impl<T: AsyncRead> Read for HttpProxyReader<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut n = self.read_local_http(buf);
        if n > 0 {
            return Ok(n);
        }
        n = self.read_body(buf);
        if n > 0 {
            return Ok(n);
        }
        loop {
            self.recv_buf.reserve(buf.len());
            let pos = self.recv_buf.len();
            let cap = self.recv_buf.capacity();
            debug!(
                "http try read {} bytes {}  {} {:?} {}!",
                cap - pos,
                pos,
                self.body_length,
                self.state,
                self.counter,
            );
            unsafe {
                self.recv_buf.set_len(cap);
            }
            //not_ready = false;
            self.counter = self.counter + 1;
            match self.reader.poll_read(&mut self.recv_buf[pos..]) {
                Ok(Async::Ready(nn)) => {
                    debug!("http Recv {} bytes. {}", nn, self.counter - 1);
                    unsafe {
                        self.recv_buf.set_len(pos + nn);
                    }
                    if 0 == nn {
                        return Ok(0);
                    }
                }
                Err(e) => {
                    return Err(e);
                }
                Ok(Async::NotReady) => {
                    debug!("http Recv not ready. {}", self.counter - 1);
                    unsafe {
                        self.recv_buf.set_len(pos);
                    }
                    return Err(Error::from(ErrorKind::WouldBlock));
                }
            }
            match &self.state {
                DECODING_HEADER => {
                    // if not_ready {
                    //     return Err(Error::from(ErrorKind::WouldBlock));
                    // }
                    match self.parse_request() {
                        Ok(0) => {
                            continue;
                        }
                        Ok(_) => {
                            return Ok(self.read_local_http(buf));
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                }
                DECODING_BODY => {
                    return Ok(self.read_body(buf));
                }
            }
        }
    }
}

pub fn is_ok_response(buf: &[u8]) -> bool {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut res = httparse::Response::new(&mut headers);
    match res.parse(buf) {
        Ok(Status::Complete(_)) => {
            //info!("code is {}", res.code.unwrap());
            res.code.unwrap() < 300
        }
        _ => false,
    }
}
