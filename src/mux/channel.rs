use crate::mux::event::*;
use crate::mux::mux::*;
use std::io::{Cursor, Error, ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use std::collections::HashMap;

use bytes::Bytes;

use tokio::io;
use tokio_io::{AsyncRead, AsyncWrite};

use tokio::codec::*;

use futures::sync::mpsc;
use tokio::prelude::*;
use tokio_sync::semaphore::{Permit, Semaphore};

struct MuxStreamState {
    stream_id: u32,
    send_buf_window: AtomicU32,
    recv_buf_window: AtomicU32,
    window_sem: Semaphore,
    closed: AtomicBool,
}

struct MuxStreamInner {
    state: Arc<MuxStreamState>,
    recv_buf: Cursor<Bytes>,
    send_channel: mpsc::Sender<Event>,
    recv_channel: mpsc::Receiver<Bytes>,
}
impl AsyncRead for MuxStreamInner {}
impl Read for MuxStreamInner {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.state.closed.load(Ordering::SeqCst) {
            return Err(Error::from(ErrorKind::ConnectionReset));
        }
        let n = self.recv_buf.read(buf)?;
        if n > 0 {
            return Ok(n);
        } else {
            let r = self.recv_channel.poll();
            match r {
                Ok(Async::NotReady) => Err(Error::from(ErrorKind::WouldBlock)),
                Ok(Async::Ready(None)) => Err(Error::from(ErrorKind::ConnectionReset)),
                Ok(Async::Ready(Some(b))) => {
                    self.recv_buf = Cursor::new(b);
                    self.recv_buf.read(buf)
                }
                Err(_) => Err(Error::from(ErrorKind::Other)),
            }
        }
    }
}
impl AsyncWrite for MuxStreamInner {
    fn shutdown(&mut self) -> Result<Async<()>, io::Error> {
        self.state.closed.store(true, Ordering::SeqCst);
        //self.conn.lock().unwrap().close_stream(self.stream_id, true);
        //self.send_channel.close();
        Ok(Async::Ready(()))
    }
}
impl Write for MuxStreamInner {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.state.closed.load(Ordering::SeqCst) {
            return Err(Error::from(ErrorKind::ConnectionReset));
        }

        let mut send_permit = Permit::new();
        let acuire = send_permit.poll_acquire(&self.state.window_sem);
        match acuire {
            Err(e) => {
                error!("failed to acuire:{}", e);
                return Err(Error::from(ErrorKind::Other));
            }
            Ok(Async::NotReady) => {
                error!("failed to acuire send window buf");
                return Err(Error::from(ErrorKind::WouldBlock));
            }
            _ => {}
        }
        let mut send_len = buf.len();
        if send_len > self.state.send_buf_window.load(Ordering::SeqCst) as usize {
            send_len = self.state.send_buf_window.load(Ordering::SeqCst) as usize;
        }
        let ev = new_data_event(self.state.stream_id, &buf[0..send_len], true);
        let r = self.send_channel.start_send(ev);
        match r {
            Ok(AsyncSink::Ready) => {
                self.state
                    .send_buf_window
                    .fetch_sub(send_len as u32, Ordering::SeqCst);
                if self.state.send_buf_window.load(Ordering::SeqCst) > 0 {
                    send_permit.release(&self.state.window_sem);
                }
                Ok(send_len)
            }
            Ok(AsyncSink::NotReady(_)) => Err(Error::from(ErrorKind::WouldBlock)),
            Err(_) => Err(Error::from(ErrorKind::Other)),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send_channel.poll_complete();
        Ok(())
    }
}

pub struct ChannelMuxStream {
    state: Arc<MuxStreamState>,
    send_channel: mpsc::Sender<Event>,
    recv_data_channel: Option<mpsc::Sender<Bytes>>,
}

impl ChannelMuxStream {
    pub fn new(id: u32, s: &mpsc::Sender<Event>) -> Self {
        let state = MuxStreamState {
            stream_id: id,
            send_buf_window: AtomicU32::new(0),
            recv_buf_window: AtomicU32::new(0),
            window_sem: Semaphore::new(1),
            closed: AtomicBool::new(false),
        };
        Self {
            state: Arc::new(state),
            send_channel: s.clone(),
            recv_data_channel: None,
        }
    }
}

impl MuxStream for ChannelMuxStream {
    fn split(&mut self) -> (Box<dyn AsyncRead + Send>, Box<dyn AsyncWrite + Send>) {
        let (send, recv) = mpsc::channel(1024);
        self.recv_data_channel = Some(send);

        let inner = MuxStreamInner {
            state: self.state.clone(),
            recv_buf: Cursor::new(Bytes::with_capacity(0)),
            send_channel: self.send_channel.clone(),
            recv_channel: recv,
        };
        let (r, w) = inner.split();
        (Box::new(r), Box::new(w))
    }
    fn close(&mut self) {}
    fn handle_recv_data(&mut self, data: Vec<u8>) {
        let data_len = data.len() as u32;
        match &mut self.recv_data_channel {
            Some(tx) => {
                tx.try_send(Bytes::from(data));
            }
            None => {
                return;
            }
        }
        let recv_window_size = self
            .state
            .recv_buf_window
            .fetch_add(data_len, Ordering::SeqCst);
        if recv_window_size >= 32 * 1024 {
            let ev = new_window_update_event(self.state.stream_id, recv_window_size, true);
            self.send_channel.start_send(ev);
            self.state
                .recv_buf_window
                .fetch_sub(recv_window_size, Ordering::SeqCst);
        }
    }
    fn handle_window_update(&mut self, len: u32) {
        self.state.send_buf_window.fetch_add(len, Ordering::SeqCst);
        if self.state.window_sem.available_permits() == 0 {
            self.state.window_sem.add_permits(1);
        }
    }
    fn id(&self) -> u32 {
        self.state.stream_id
    }
}

pub struct ChannelMuxSession {
    event_trigger_send: mpsc::Sender<Event>,
    streams: HashMap<u32, ChannelMuxStream>,
}

impl ChannelMuxSession {
    pub fn new(send: &mpsc::Sender<Event>) -> Self {
        Self {
            event_trigger_send: send.clone(),
            streams: HashMap::new(),
        }
    }
}

impl MuxSession for ChannelMuxSession {
    fn new_stream(&mut self, sid: u32) -> &mut MuxStream {
        let s = ChannelMuxStream::new(sid, &self.event_trigger_send);
        self.streams.entry(sid).or_insert(s)
    }
    fn get_stream(&mut self, sid: u32) -> Option<&mut MuxStream> {
        match self.streams.get_mut(&sid) {
            Some(s) => Some(s),
            None => None,
        }
    }
    fn open_stream(&mut self, proto: &str, addr: &str) -> &mut MuxStream {
        self.new_stream(1)
    }
    fn close_stream(&mut self, sid: u32, initial: bool) {}
}
