use std::io as std_io;
use std::io::{Error, ErrorKind, Read, Write};
use tokio::net::UdpSocket;
use tokio::prelude::*;
use tokio_io::{AsyncRead, AsyncWrite};

pub struct UdpConnection {
    sock: UdpSocket,
}

impl UdpConnection {
    pub fn new(sock: UdpSocket) -> Self {
        Self { sock: sock }
    }
}

impl AsyncRead for UdpConnection {}
impl Read for UdpConnection {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.sock.poll_recv(buf) {
            Ok(Async::Ready(n)) => Ok(n),
            Ok(Async::NotReady) => Err(Error::from(ErrorKind::WouldBlock)),
            Err(e) => Err(e),
        }
    }
}

impl AsyncWrite for UdpConnection {
    fn shutdown(&mut self) -> Poll<(), std_io::Error> {
        Ok(Async::Ready(()))
    }
}

impl Write for UdpConnection {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.poll_write(buf) {
            Ok(Async::Ready(n)) => Ok(n),
            Ok(Async::NotReady) => Err(Error::from(ErrorKind::WouldBlock)),
            Err(e) => Err(e),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
