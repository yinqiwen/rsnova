#![feature(integer_atomics)]
#[macro_use]

use crate::mux::message::ConnectRequest;
use crate::mux::event::*;
use crate::mux::manager::*;
use crate::mux::stream::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use std::collections::HashMap;

//use tokio::io::write_all;
use tokio_io::{AsyncRead, AsyncWrite};

use bytes::BytesMut;
use tokio::codec::*;

use tokio::io;
use tokio::net::TcpStream;
use tokio::prelude::*;
use tokio_io::io::copy;

pub struct CommonMuxConn<T: AsyncRead + AsyncWrite> {
    streams: HashMap<u32, CommonMuxStream<T>>,
    writer: stream::SplitSink<Framed<T, EventCodec>>,
    reader: stream::SplitStream<Framed<T, EventCodec>>,
}

impl<T: AsyncRead + AsyncWrite> CommonMuxConn<T> {
    pub fn new_stream(&mut self, id: u32, conn: &ConnRef<T>) -> &mut CommonMuxStream<T> {
        let s = CommonMuxStream::new(id, conn);
        self.streams.entry(id).or_insert(s)
    }

    fn get_stream(&mut self, id: u32) -> Option<&mut CommonMuxStream<T>> {
        self.streams.get_mut(&id)
    }

    pub fn write_event(&mut self, ev: Event) -> Result<AsyncSink<Event>, io::Error> {
        self.writer.start_send(ev)
    }

}

pub struct ConnRef<T: AsyncRead + AsyncWrite>(Arc<CommonMuxConn<T>>);
impl<T: AsyncRead + AsyncWrite> Clone for ConnRef<T> {
    fn clone(&self) -> Self {
        //self.0.lock().unwrap().ref_count += 1;
        Self(self.0.clone())
    }
}

// impl<T: AsyncRead + AsyncWrite> Drop for ConnRef<T> {
//     fn drop(&mut self) {
//         let conn = &mut *self.0.lock().unwrap();
//         if let Some(x) = conn.ref_count.checked_sub(1) {
//             conn.ref_count = x;
//             if x == 0
//                 && !conn.inner.is_closed()
//                 && conn.uni_opening.is_empty()
//                 && conn.bi_opening.is_empty()
//             {
//                 // If the driver is alive, it's just it and us, so we'd better shut it down. If it's
//                 // not, we can't do any harm. If there were any streams being opened, then either
//                 // the connection will be closed for an unrelated reason or a fresh reference will
//                 // be constructed for the newly opened stream.
//                 conn.implicit_close();
//             }
//         }
//     }
// }

impl<T: AsyncRead + AsyncWrite> std::ops::Deref for ConnRef<T> {
    type Target = CommonMuxConn<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: AsyncRead + AsyncWrite> ConnRef<T> {
    pub(crate) fn new(conn: T) -> Self {
        let (writer, reader) = Framed::new(conn, EventCodec::new()).split();
        Self(Arc::new(CommonMuxConn {
            streams: HashMap::new(),
            writer: writer,
            reader: reader,
        }))
    }
}

pub struct CommonMuxSession<T: AsyncRead + AsyncWrite> {
    conn: ConnRef<T>,
}

impl<T: AsyncRead + AsyncWrite> MuxSession for CommonMuxSession<T> {
    fn new_stream(&mut self, sid: u32) -> &MuxStream {
        self.conn.new_stream(sid, &self.conn)
    }
    fn get_stream(&mut self, sid: u32) -> Option<&mut MuxStream> {
        let r = self.conn.get_stream(sid);
        match r {
            Some(s) => Some(s),
            None => None,
        }
    }
}


impl<T: AsyncRead + AsyncWrite> CommonMuxSession<T> {
    pub fn new(conn: T) -> Self {
        Self {
            conn: ConnRef::new(conn),
        }
    }

    fn process_conn_events(&mut self) -> Result<(), io::Error> {
        loop {
            match self.conn.reader.poll().expect("error msg") {
                Async::Ready(Some(ev)) => {
                    //self.get_stream(ev.header.sid);
                    self.handle_mux_event(ev);
                }
                Async::Ready(None) => {
                    return Ok(());
                }
                Async::NotReady => {
                    return Ok(());
                }
            }
        }
    }
}

