use tokio::net::TcpStream;
use tokio::prelude::*;

use bytes::Buf;
use bytes::BufMut;

use std::net::Shutdown;

use std::cell::UnsafeCell;
use std::sync::Arc;

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
    fn poll_read(&mut self, buf: &mut [u8]) -> Poll<usize, std::io::Error> {
        self.inner.poll_read(buf)
    }
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.inner.prepare_uninitialized_buffer(buf)
    }

    fn read_buf<B: BufMut>(&mut self, buf: &mut B) -> Poll<usize, std::io::Error> {
        self.inner.read_buf(buf)
    }
}

impl AsyncWrite for MyTcpStream {
    fn poll_write(&mut self, buf: &[u8]) -> Poll<usize, std::io::Error> {
        self.inner.poll_write(buf)
    }
    fn poll_flush(&mut self) -> Poll<(), std::io::Error> {
        self.inner.poll_flush()
    }
    fn shutdown(&mut self) -> Poll<(), std::io::Error> {
        self.inner.shutdown(Shutdown::Write)?;
        Ok(().into())
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, std::io::Error> {
        self.inner.write_buf(buf)
    }
}

#[derive(Debug)]
pub struct TcpStreamRecv {
    inner: Arc<UnsafeCell<TcpStream>>,
}

unsafe impl Send for TcpStreamRecv {}
unsafe impl Sync for TcpStreamRecv {}

impl TcpStreamRecv {
    pub fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        unsafe { &*self.inner.get() }.shutdown(how)
    }
}

impl Read for TcpStreamRecv {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        unsafe { &mut *self.inner.get() }.read(buf)
    }
}

impl AsyncRead for TcpStreamRecv {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        (&*self.inner.get()).prepare_uninitialized_buffer(buf)
    }

    fn read_buf<B: bytes::BufMut>(&mut self, buf: &mut B) -> Poll<usize, std::io::Error> {
        unsafe { &mut *self.inner.get() }.read_buf(buf)
    }
}

#[derive(Debug)]
pub struct TcpStreamSend {
    inner: Arc<UnsafeCell<TcpStream>>,
}

unsafe impl Send for TcpStreamSend {}
unsafe impl Sync for TcpStreamSend {}

impl TcpStreamSend {
    fn shutdown_r(&self, how: Shutdown) -> std::io::Result<()> {
        unsafe { &*self.inner.get() }.shutdown(how)
    }
}

impl Write for TcpStreamSend {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        unsafe { &mut *self.inner.get() }.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        unsafe { &mut *self.inner.get() }.flush()
    }
}

impl AsyncWrite for TcpStreamSend {
    fn shutdown(&mut self) -> Poll<(), std::io::Error> {
        self.shutdown_r(Shutdown::Write)?;
        //self.shutdown(Shutdown::Write);
        //unsafe { &mut *self.inner.get() }.shutdown()
        // self.shutdown(Shutdown::Write);
        Ok(().into())
    }

    fn write_buf<B: bytes::Buf>(&mut self, buf: &mut B) -> Poll<usize, std::io::Error> {
        unsafe { &mut *self.inner.get() }.write_buf(buf)
    }
}

pub fn tcp_split(stream: TcpStream) -> (TcpStreamRecv, TcpStreamSend) {
    let inner = Arc::new(UnsafeCell::new(stream));
    let send = TcpStreamSend {
        inner: inner.clone(),
    };
    let recv = TcpStreamRecv {
        inner: inner.clone(),
    };
    (recv, send)
}
