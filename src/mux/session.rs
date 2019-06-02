#![feature(integer_atomics)]
#[macro_use]
use crate::mux::event::*;
use crate::mux::manager::*;
use crate::mux::stream::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use std::collections::HashMap;

//use tokio::io::write_all;
use tokio_io::{AsyncRead, AsyncWrite};

use tokio::codec::*;

use tokio::io;
use tokio::prelude::*;

pub struct CommonMuxConn<T: AsyncRead + AsyncWrite + Send + 'static> {
    writer: stream::SplitSink<Framed<T, EventCodec>>,
    reader: stream::SplitStream<Framed<T, EventCodec>>,
    streams: HashMap<u32, Arc<Mutex<CommonMuxStream<T>>>>,
}

impl<T: AsyncRead + AsyncWrite + Send + 'static> CommonMuxConn<T> {
    pub fn write_event(&mut self, ev: Event) -> Result<AsyncSink<Event>, io::Error> {
        self.writer.start_send(ev)
    }
    pub fn close(&mut self) {
        self.streams.retain(|_, s| {
            s.lock().unwrap().close();
            true
        });
        self.writer.close();
    }
    pub fn close_stream(&mut self, sid: u32, initial: bool) {
        match self.streams.remove(&sid) {
            Some(s) => {
                s.lock().unwrap().close();
                if initial {
                    let ev = new_fin_event(sid);
                    self.write_event(ev);
                }
            }
            None => {}
        }
    }
    fn new_stream(&mut self, sid: u32, conn: &ConnRef<T>) -> Arc<Mutex<dyn MuxStream>> {
        let s = Arc::new(Mutex::new(CommonMuxStream::new(sid, conn)));
        self.streams.entry(sid).or_insert(s).clone()
    }
    fn get_stream(&mut self, sid: u32) -> Option<Arc<Mutex<dyn MuxStream>>> {
        let r = self.streams.get_mut(&sid);
        match r {
            Some(s) => Some(s.clone()),
            None => None,
        }
    }
    pub fn flush(&mut self) {
        self.writer.poll_complete();
    }
}

pub struct ConnRef<T: AsyncRead + AsyncWrite + Send + 'static>(Arc<Mutex<CommonMuxConn<T>>>);
impl<T: AsyncRead + AsyncWrite + Send> Clone for ConnRef<T> {
    fn clone(&self) -> Self {
        //self.0.lock().unwrap().ref_count += 1;
        Self(self.0.clone())
    }
}

impl<T: AsyncRead + AsyncWrite + Send + 'static> std::ops::Deref for ConnRef<T> {
    type Target = Mutex<CommonMuxConn<T>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: AsyncRead + AsyncWrite + Send + 'static> ConnRef<T> {
    pub(crate) fn new(conn: T) -> Self {
        let (writer, reader) = Framed::new(conn, EventCodec::new()).split();
        Self(Arc::new(Mutex::new(CommonMuxConn {
            writer: writer,
            reader: reader,
            streams: HashMap::new(),
        })))
    }
}

pub struct CommonMuxSession<T: AsyncRead + AsyncWrite + Send + 'static> {
    conn: ConnRef<T>,
}

impl<T: AsyncRead + AsyncWrite + Send + 'static> MuxSession for CommonMuxSession<T> {
    fn close_stream(&mut self, sid: u32, initial: bool) {
        self.conn.lock().unwrap().close_stream(sid, initial);
    }
    fn new_stream(&mut self, sid: u32) -> Arc<Mutex<dyn MuxStream>> {
        self.conn.lock().unwrap().new_stream(sid, &self.conn)
    }
    fn get_stream(&mut self, sid: u32) -> Option<Arc<Mutex<dyn MuxStream>>> {
        self.conn.lock().unwrap().get_stream(sid)
    }

    fn process_conn_events(&mut self) -> Result<(), io::Error> {
        loop {
            let r = self.conn.lock().unwrap().reader.poll();
            match r {
                Ok(Async::Ready(Some(ev))) => {
                    //self.get_stream(ev.header.sid);
                    self.handle_mux_event(ev);
                }
                Ok(Async::Ready(None)) => {
                    return Ok(());
                }
                Ok(Async::NotReady) => {
                    return Ok(());
                }
                Err(e) => {
                    self.shutdown();
                    return Err(e);
                }
            }
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Send + 'static> CommonMuxSession<T> {
    pub fn new(conn: T) -> Self {
        Self {
            conn: ConnRef::new(conn),
        }
    }

    fn shutdown(&mut self) {
        self.conn.lock().unwrap().close();
    }
}
