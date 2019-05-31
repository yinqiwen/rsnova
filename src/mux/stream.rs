use crate::mux::event::*;
use crate::mux::manager::*;
use crate::mux::session::*;

use std::collections::VecDeque;

use std::io;
use std::io::{Cursor, Read, Write};

use bytes::Bytes;
use std::rc::Rc;

use std::cell::RefCell;

use tokio::prelude::*;
use tokio_io::{AsyncRead, AsyncWrite};
pub struct CommonMuxStream<T: AsyncRead + AsyncWrite> {
    pub sid: u32,
    pub recv_buf: Rc<RefCell<VecDeque<Cursor<Bytes>>>>,
    pub conn: ConnRef<T>,
}

impl<T: AsyncRead + AsyncWrite> CommonMuxStream<T> {
    pub fn new(id: u32, conn: &ConnRef<T>) -> Self {
        Self {
            sid: id,
            recv_buf: Rc::new(RefCell::new(VecDeque::new())),
            conn: conn.clone(),
        }
    }
}


impl<T: AsyncRead + AsyncWrite> MuxStream for CommonMuxStream<T> {
    fn handle_recv_data(&mut self, data: Vec<u8>) {
        let buf = Bytes::from(data);
        self.recv_buf.borrow_mut().push_back(Cursor::new(buf));
    }

    fn split<'a>(&'a self) -> (Box<dyn AsyncRead + 'a>, Box<dyn AsyncWrite + 'a>) {
        let recv = RecvStream {
            rbuf: self.recv_buf.clone(),
        };
        let send = SendStream {
            stream_id: self.sid,
            conn: self.conn.clone(),
        };
        (Box::new(recv), Box::new(send))
    }
    fn close(&mut self) {}

}


struct SendStream<T: AsyncRead + AsyncWrite> {
    pub stream_id: u32,
    pub conn: ConnRef<T>,
}

struct RecvStream {
    pub rbuf: Rc<RefCell<VecDeque<Cursor<Bytes>>>>,
}


impl AsyncRead for RecvStream {}
impl Read for RecvStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut rbuf = self.rbuf.borrow_mut();
        loop {
            let r = rbuf.front_mut();
            match self.rbuf.borrow_mut().front_mut() {
                Some(chunk) => {
                    return chunk.read(buf);
                }
                None => return Ok(0),
            };
        }
        Ok(0)
    }
}

impl<T: AsyncRead + AsyncWrite> AsyncWrite for SendStream<T> {
    fn shutdown(&mut self) -> Result<Async<()>, io::Error> {
        Ok(Async::Ready(()))
    }
}

impl<T: AsyncRead + AsyncWrite> Write for SendStream<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let ev = new_data_event(self.stream_id, buf);
        self.conn.write_event(ev)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        //(&self.io).flush()
        Ok(())
    }
}
