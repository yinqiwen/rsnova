use tokio::net::TcpStream;
use tokio::prelude::*;

use bytes::Buf;
use bytes::BufMut;

use std::net::Shutdown;

pub struct MyTcpStream {
    inner: TcpStream,
}

impl MyTcpStream {
    pub fn new(s: TcpStream) -> Self {
        Self { inner: s }
    }
}

impl Read for MyTcpStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for MyTcpStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl AsyncRead for MyTcpStream {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.inner.prepare_uninitialized_buffer(buf)
    }

    fn read_buf<B: BufMut>(&mut self, buf: &mut B) -> Poll<usize, std::io::Error> {
        self.inner.read_buf(buf)
    }
}

impl AsyncWrite for MyTcpStream {
    fn shutdown(&mut self) -> Poll<(), std::io::Error> {
        self.inner.shutdown(Shutdown::Write)?;
        Ok(().into())
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, std::io::Error> {
        self.inner.write_buf(buf)
    }
}
