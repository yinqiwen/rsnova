use super::relay::{relay_connection, relay_stream};
use crate::utils::{make_error, read_until_separator};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use httparse::Status;
use std::collections::vec_deque::VecDeque;
use std::error::Error;
use std::fmt::Write as fmt_write;
use std::net::Shutdown;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncRead;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use unicase::Ascii;

use crate::config::TunnelConfig;
use crate::utils::fill_read_buf;

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
            let _ = buf.write_str(v.as_str());
            let _ = buf.write_char(' ');
        }
        if let Some(v) = &self.path {
            buf.reserve(v.len() + 1);
            let _ = buf.write_str(v.as_str());
            let _ = buf.write_char(' ');
        }
        if let Some(v) = &self.version {
            buf.reserve(10);
            if *v == 0 {
                let _ = buf.write_str("HTTP/1.0");
            } else {
                let _ = buf.write_str("HTTP/1.1");
            }
        }
        buf.reserve(2);
        let _ = buf.write_str("\r\n");
        for h in &self.headers {
            buf.reserve(h.name.len() + 1 + h.value.len() + 2);
            let _ = buf.write_str(h.name.as_str());
            let _ = buf.write_char(':');
            buf.reserve(h.value.len() + 2);
            buf.put_slice(&h.value[..]);
            let _ = buf.write_str("\r\n");
        }
        buf.reserve(2);
        let _ = buf.write_str("\r\n");
        buf.freeze()
    }
}

pub enum HttpMessage {
    Request(HttpRequest),
    Chunk(Bytes),
}
#[derive(Debug, PartialEq)]
enum HttpDecodeState {
    DecodingHeader,
    DecodingBody,
}

pub struct HttpReader<'a, T: ?Sized> {
    reader: &'a mut T,
    state: HttpDecodeState,
    recv_buf: BytesMut,
    http_buf: BytesMut,
    body_length: i64,
    body_tail: VecDeque<u8>,
    //remote_host: String,
    counter: u32,
}

fn newHttpReader<'a, T>(sock: &'a mut T) -> HttpReader<'a, T>
where
    T: AsyncRead + Unpin + ?Sized,
{
    HttpReader {
        reader: sock,
        state: HttpDecodeState::DecodingHeader,
        recv_buf: BytesMut::new(),
        http_buf: BytesMut::new(),
        body_length: 0,
        body_tail: VecDeque::new(),
        //remote_host: String::new(),
        counter: 0,
    }
}

impl<T: AsyncRead + Unpin + ?Sized> HttpReader<'_, T> {
    // pub fn get_remote_addr(&self) -> &str {
    //     self.remote_host.as_str()
    // }
    pub fn add_recv_content(&mut self, b: &[u8]) {
        self.recv_buf.reserve(b.len());
        self.recv_buf.put_slice(b);
    }
    fn parse_request(&mut self) -> Result<(bool, String, i64), std::io::Error> {
        parse_request(&mut self.recv_buf, Some(&mut self.http_buf))
    }
}

fn check_body_complete_and_fill_read_buf(
    recv_buf: &mut BytesMut,
    dst: &mut [u8],
    tail: &mut VecDeque<u8>,
    body_len: &mut i64,
) -> (bool, usize) {
    let mut body_complete = false;
    let mut fill_n: usize = 0;
    if *body_len < 0 {
        let mut tail_pos = 0;
        if recv_buf.len() > 5 {
            tail_pos = recv_buf.len() - 5;
            tail.clear();
        }
        while tail_pos < recv_buf.len() {
            tail.push_back(recv_buf[tail_pos]);
            tail_pos = tail_pos + 1;
            if tail.len() > 5 {
                tail.pop_front();
            }
        }
        if tail.len() == 5
            && tail[0] == '0' as u8
            && tail[1] == '\r' as u8
            && tail[2] == '\n' as u8
            && tail[3] == '\r' as u8
            && tail[4] == '\n' as u8
        {
            body_complete = true;
        }
        fill_n = fill_read_buf(recv_buf, dst);
    } else if *body_len > 0 {
        fill_n = fill_read_buf(recv_buf, dst);
        *body_len = *body_len - fill_n as i64;
        if *body_len == 0 {
            body_complete = true;
        }
    }
    (body_complete, fill_n)
}

fn parse_request(
    recv_buf: &mut BytesMut,
    http_buf: Option<&mut BytesMut>,
) -> Result<(bool, String, i64), std::io::Error> {
    let mut success = false;
    let mut body_length: i64 = 0;
    let mut remote_host = String::from("");
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    let mut header_len: usize = 0;
    let data = &mut recv_buf[..];
    match req.parse(data) {
        Ok(Status::Complete(n)) => {
            //self.state = HttpDecodeState::DecodingBody;
            //self.body_tail.clear();
            header_len = n;
            success = true;
        }
        Ok(Status::Partial) => {
            return Ok((success, remote_host, body_length));
        }
        Err(_) => {
            return Err(std::io::Error::from(std::io::ErrorKind::Other));
        }
    };
    let mut hreq: HttpRequest = Default::default();
    for h in req.headers {
        let mut header = Header {
            name: Ascii::new(String::from(h.name)),
            value: Bytes::copy_from_slice(h.value),
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
                    body_length = -1;
                }
            }
            "content-length" => {
                match String::from_utf8_lossy(&header.value[..]).parse::<u64>() {
                    Ok(v) => {
                        body_length = v as i64;
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
                remote_host = String::from(String::from_utf8_lossy(h.value));
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
    if let Some(hbuf) = http_buf {
        hbuf.clear();
        hbuf.reserve(b.len());
        hbuf.put_slice(&b[..]);
    }

    recv_buf.advance(header_len);
    if 0 == body_length {
        //self.switch_reading_header();
        //info!("switch to reading header while data len:{}", recv_buf.len());
    }
    Ok((success, remote_host, body_length))
}

impl<T: AsyncRead + ?Sized + Unpin> AsyncRead for HttpReader<'_, T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let Self {
            reader,
            state,
            recv_buf,
            http_buf,
            body_length,
            body_tail,
            //remote_host,
            counter,
        } = &mut *self;
        let pr = Pin::new(reader);
        let mut n = fill_read_buf(http_buf, buf);
        if n > 0 {
            return Poll::Ready(Ok(n));
        }
        if *state == HttpDecodeState::DecodingBody {
            let (complete, n) =
                check_body_complete_and_fill_read_buf(recv_buf, buf, body_tail, body_length);
            if complete {
                *state = HttpDecodeState::DecodingHeader;
            }
            if n > 0 {
                return Poll::Ready(Ok(n));
            }
        }
        recv_buf.reserve(buf.len());
        let pos = recv_buf.len();
        let cap = recv_buf.capacity();
        unsafe {
            recv_buf.set_len(cap);
        }
        match pr.poll_read(cx, &mut recv_buf[pos..]) {
            Poll::Ready(Ok(nn)) => {
                unsafe {
                    recv_buf.set_len(pos + nn);
                }
                if 0 == nn {
                    return Poll::Ready(Ok(0));
                }
            }
            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(e));
            }
            Poll::Pending => {
                debug!("http Recv not ready. {}", *counter - 1);
                unsafe {
                    recv_buf.set_len(pos);
                }
                return Poll::Pending;
            }
        }
        if *state == HttpDecodeState::DecodingHeader {
            match parse_request(recv_buf, Some(http_buf)) {
                Ok((success, target, blen)) => {
                    if success {
                        if 0 == blen {
                            *state = HttpDecodeState::DecodingHeader;
                        } else {
                            *state = HttpDecodeState::DecodingBody;
                        }
                        //*remote_host = target;
                        *body_length = blen;
                        let n = fill_read_buf(http_buf, buf);
                        return Poll::Ready(Ok(n));
                    }
                }
                Err(e) => {
                    return Poll::Ready(Err(e));
                }
            }
        } else {
            let (complete, n) =
                check_body_complete_and_fill_read_buf(recv_buf, buf, body_tail, body_length);
            if complete {
                *state = HttpDecodeState::DecodingHeader;
            }
            return Poll::Ready(Ok(n));
        }
        Poll::Ready(Ok(0))
    }
}

pub async fn handle_http(
    tunnel_id: u32,
    mut inbound: TcpStream,
    cfg: &TunnelConfig,
) -> Result<(), Box<dyn Error>> {
    let (head, body) = read_until_separator(&mut inbound, "\r\n\r\n").await?;

    let (mut ri, mut wi) = inbound.split();
    let mut hreader = newHttpReader(&mut ri);
    hreader.add_recv_content(&head);
    hreader.add_recv_content(&body);
    let mut target = String::new();
    match hreader.parse_request() {
        Err(e) => {
            return Err(make_error("failed to parse http header"));
        }
        Ok((success, remote, _)) => {
            if !success {
                return Err(make_error("failed to parse http header complete"));
            }
            target = remote;
        }
    }
    if target.find(':').is_none() {
        target.push_str(":80");
    }
    info!("[{}]Handle HTTP proxy to {} ", tunnel_id, target);
    relay_stream(tunnel_id, &mut hreader, &mut wi, target, cfg, Vec::new()).await?;
    let _ = inbound.shutdown(Shutdown::Both);
    Ok(())
}

pub async fn handle_https(
    tunnel_id: u32,
    mut inbound: TcpStream,
    cfg: &TunnelConfig,
) -> Result<(), Box<dyn Error>> {
    let mut target = String::new();
    let (head, _) = read_until_separator(&mut inbound, "\r\n\r\n").await?;
    let mut hbuf = BytesMut::from(&head[..]);
    match parse_request(&mut hbuf, None) {
        Err(e) => {
            return Err(make_error("failed to parse http header"));
        }
        Ok((success, remote, _)) => {
            if !success {
                return Err(make_error("failed to parse http header complete"));
            }
            target = remote;
        }
    }

    let conn_res = "HTTP/1.0 200 Connection established\r\n\r\n";
    inbound.write_all(conn_res.as_bytes()).await?;

    info!("[{}]Handle HTTPS proxy to {} ", tunnel_id, target);
    relay_connection(tunnel_id, inbound, cfg, target, Vec::new()).await?;
    Ok(())
}
