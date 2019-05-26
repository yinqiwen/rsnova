use crate::mux::event::*;
use crate::mux::session::MuxSession;

use std::io;
use std::io::{Cursor, Read, Write};

use bytes::BytesMut;
use tokio::prelude::*;
use tokio_io::{AsyncRead, AsyncWrite};

pub struct MuxStream<T: AsyncRead + AsyncWrite> {
    pub sid: u32,
    pub rbuf: BytesMut,
    pub session: *mut MuxSession<T>,
    //pub session: S,
}

// pub fn handle_event(stream: Option<&MuxStream>, ev: Event) {}

// impl<T: AsyncRead + AsyncWrite> AsyncRead for MuxStream<T> {}

// impl<T: AsyncRead + AsyncWrite> AsyncWrite for MuxStream<T> {
//     fn shutdown(&mut self) -> Poll<(), io::Error> {
//         //let session = unsafe { &mut *self.session };
//         //session.transport.
//     }
// }

impl<T: AsyncRead + AsyncWrite> Read for MuxStream<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut src = Cursor::new(&mut self.rbuf);
        src.read(buf)
    }
}

impl<T: AsyncRead + AsyncWrite> Write for MuxStream<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let ev = new_data_event(self.sid, buf);
        //(&self.io).write(buf)
        let session = unsafe { &mut *self.session };
        session.write_event(ev)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        //(&self.io).flush()

        Ok(())
    }
}
