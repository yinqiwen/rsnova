#[cfg(target_os = "linux")]
use cfg_if::cfg_if;
use std::{
    future::Future,
    io::{self},
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(target_os = "linux")]
use std::{
    io::{Error, ErrorKind},
    mem,
    os::fd::AsRawFd,
    ptr,
};

use bytes::{Bytes, BytesMut};
#[cfg(target_os = "linux")]
use futures::ready;
#[cfg(target_os = "linux")]
use socket2::SockAddr;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::UdpSocket,
    sync::mpsc,
};

#[cfg(target_os = "linux")]
use crate::utils::net::get_destination_addr;

use super::fill_read_buf;

pub struct UdpClientStream {
    socket: UdpSocket,
}

impl UdpClientStream {
    pub fn new(socket: UdpSocket) -> Self {
        Self { socket }
    }
}

impl AsyncRead for UdpClientStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.socket.poll_recv(cx, buf)
    }
}
impl AsyncWrite for UdpClientStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.socket.poll_send(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[allow(dead_code)]
pub struct UdpServerStream {
    recv_buf: BytesMut,
    receiver: mpsc::Receiver<Bytes>,
    tunnel_sender: mpsc::Sender<(Bytes, SocketAddr)>,

    addr: SocketAddr,
}

impl UdpServerStream {
    #[allow(dead_code)]
    pub fn new(
        receiver: mpsc::Receiver<Bytes>,
        tunnel_sender: mpsc::Sender<(Bytes, SocketAddr)>,
        addr: SocketAddr,
    ) -> Self {
        Self {
            recv_buf: BytesMut::new(),
            receiver,
            tunnel_sender,
            addr,
        }
    }
}
impl AsyncRead for UdpServerStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.recv_buf.is_empty() {
            fill_read_buf(&mut self.recv_buf, buf);
            return Poll::Ready(Ok(()));
        };

        match self.receiver.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let mut copy_n: usize = data.len();
                if 0 == copy_n {
                    return Poll::Ready(Ok(()));
                }
                if copy_n > buf.remaining() {
                    copy_n = buf.remaining();
                }
                buf.put_slice(&data[0..copy_n]);
                if copy_n < data.len() {
                    self.recv_buf.extend_from_slice(&data[copy_n..]);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "close by remote",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}
impl AsyncWrite for UdpServerStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[allow(dead_code)]
pub trait TproxyUdpSocket {
    /// Receive a single datagram from the socket.
    ///
    /// On success, the future resolves to the number of bytes read and the source, target address
    ///
    /// `(bytes read, source address, target address)`
    fn poll_recv_dest_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr, SocketAddr)>>;
}

#[cfg(target_os = "linux")]
pub struct LinuxTproxyUdpSocket {
    socket: tokio::io::unix::AsyncFd<tokio::net::UdpSocket>,
    // socket2: ManuallyDrop<socket2::Socket>,
}

#[cfg(target_os = "linux")]
impl LinuxTproxyUdpSocket {
    pub fn new(socket: tokio::net::UdpSocket) -> io::Result<Self> {
        // let socket2 =
        //     ManuallyDrop::new(unsafe { socket2::Socket::from_raw_fd(socket.as_raw_fd()) });
        let async_socket = tokio::io::unix::AsyncFd::new(socket)?;
        Ok(Self {
            socket: async_socket,
        })
    }

    pub fn recv_dest_from<'a>(&'a self, buf: &'a mut [u8]) -> RecvDestFrom<'a, Self>
    where
        Self: Sized,
    {
        RecvDestFrom { socket: self, buf }
    }
}
#[cfg(target_os = "linux")]
impl TproxyUdpSocket for LinuxTproxyUdpSocket {
    fn poll_recv_dest_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr, SocketAddr)>> {
        loop {
            let mut read_guard = ready!(self.socket.poll_read_ready(cx))?;
            match recv_dest_from(self.socket.get_ref(), buf) {
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    read_guard.clear_ready();
                }
                x => return Poll::Ready(x),
            }
        }
    }
}

#[allow(dead_code)]
pub struct RecvDestFrom<'a, S: 'a> {
    socket: &'a S,
    buf: &'a mut [u8],
}

impl<'a, S: 'a> Future for RecvDestFrom<'a, S>
where
    S: TproxyUdpSocket,
{
    type Output = io::Result<(usize, SocketAddr, SocketAddr)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.socket.poll_recv_dest_from(cx, self.buf)
    }
}

#[cfg(target_os = "linux")]
fn recv_dest_from(
    socket: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, SocketAddr)> {
    unsafe {
        let mut control_buf = [0u8; 64];
        let mut src_addr: libc::sockaddr_storage = mem::zeroed();

        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_name = &mut src_addr as *mut _ as *mut _;
        msg.msg_namelen = mem::size_of_val(&src_addr) as libc::socklen_t;

        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut _,
            iov_len: buf.len() as libc::size_t,
        };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;

        msg.msg_control = control_buf.as_mut_ptr() as *mut _;
        cfg_if! {
            if #[cfg(any(target_env = "musl", all(target_env = "uclibc", target_arch = "arm")))] {
                msg.msg_controllen = control_buf.len() as libc::socklen_t;
            } else {
                msg.msg_controllen = control_buf.len() as libc::size_t;
            }
        }

        let fd = socket.as_raw_fd();
        let ret = libc::recvmsg(fd, &mut msg, 0);
        if ret < 0 {
            return Err(Error::last_os_error());
        }

        let (_, src_saddr) = SockAddr::try_init(|a, l| {
            ptr::copy_nonoverlapping(msg.msg_name, a as *mut _, msg.msg_namelen as usize);
            *l = msg.msg_namelen;
            Ok(())
        })?;

        Ok((
            ret as usize,
            src_saddr.as_socket().expect("SocketAddr"),
            get_destination_addr(&msg)?,
        ))
    }
}
