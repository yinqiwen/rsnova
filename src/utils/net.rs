use super::io::read_until_separator;

use httparse::Status;
use nix::sys::socket::{getsockopt, sockopt, InetAddr};
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use url::Url;

#[cfg(not(any(target_os = "android", target_os = "linux")))]
pub fn get_origin_dst(_socket: &TcpStream) -> Option<SocketAddr> {
    None
}

#[cfg(any(target_os = "android", target_os = "linux"))]
pub fn get_origin_dst(socket: &TcpStream) -> Option<SocketAddr> {
    let fd = socket.as_raw_fd();
    let opt = sockopt::OriginalDst {};
    match getsockopt(fd, opt) {
        Ok(addr) => Some(InetAddr::V4(addr).to_std()),
        Err(_) => None,
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

pub async fn http_proxy_connect(proxy: &Url, remote: &str) -> Result<TcpStream, std::io::Error> {
    let connect_str = format!(
        "CONNECT {} HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\nProxy-Connection: keep-alive\r\n\r\n",
        remote, remote
    ).into_bytes();
    let raddr: Vec<SocketAddr> = match proxy.socket_addrs(|| None) {
        Ok(m) => m,
        Err(err) => {
            error!(
                "Failed to parse addr with error:{} from connect request:{}",
                err, proxy
            );
            return Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted));
        }
    };
    let conn = TcpStream::connect(&raddr[0]);
    let dur = std::time::Duration::from_secs(3);
    let s = tokio::time::timeout(dur, conn).await?;

    let mut socket = match s {
        Ok(s) => s,
        Err(err) => {
            return Err(err);
        }
    };
    socket.write_all(&connect_str[..]).await?;
    let (head, _) = read_until_separator(&mut socket, "\r\n\r\n").await?;
    if is_ok_response(&head[..]) {
        return Ok(socket);
    }
    Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted))
}

pub struct AsyncTcpStream {
    s: TcpStream,
}

impl AsyncTcpStream {
    pub fn new(s: TcpStream) -> Self {
        Self { s }
    }
}

impl futures::AsyncRead for AsyncTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<futures::io::Result<usize>> {
        let Self { s } = &mut *self;
        pin_mut!(s);
        s.poll_read(cx, buf)
    }
}

impl futures::AsyncWrite for AsyncTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<futures::io::Result<usize>> {
        let Self { s } = &mut *self;
        pin_mut!(s);
        s.poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let Self { s } = &mut *self;
        pin_mut!(s);
        s.poll_flush(cx)
    }
    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let Self { s } = &mut *self;
        pin_mut!(s);
        s.poll_shutdown(cx)
    }
}
