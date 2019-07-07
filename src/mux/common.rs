use crate::mux::event::*;
use crate::mux::mux::*;
use std::io::{Cursor, Error, ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use std::collections::HashMap;

use bytes::Bytes;

use tokio::io;
use tokio_io::{AsyncRead, AsyncWrite};

use tokio::prelude::*;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender, UnboundedSender};
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
    send_channel: mpsc::UnboundedSender<Event>,
    recv_channel: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl MuxStreamInner {
    fn close(&mut self) {
        // info!(
        //     "[{}]close stream:{}",
        //     self.state.stream_id,
        //     self.state.closed.load(Ordering::SeqCst)
        // );
        if !self.state.closed.load(Ordering::SeqCst) {
            self.state.closed.store(true, Ordering::SeqCst);
            self.send_channel
                .start_send(new_fin_event(self.state.stream_id));
            self.send_channel.poll_complete();
            self.state.window_sem.close();
        }
    }
}

impl AsyncRead for MuxStreamInner {}
impl Read for MuxStreamInner {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.state.closed.load(Ordering::SeqCst) {
            return Ok(0);
        }
        let n = self.recv_buf.read(buf)?;
        if n > 0 {
            return Ok(n);
        } else {
            let r = self.recv_channel.poll();
            //info!("recn poll {:?}", r);
            match r {
                Ok(Async::NotReady) => Err(Error::from(ErrorKind::WouldBlock)),
                Ok(Async::Ready(None)) => Err(Error::from(ErrorKind::ConnectionReset)),
                Ok(Async::Ready(Some(b))) => {
                    // info!(
                    //     "[{}]Got recv data with len:{}",
                    //     self.state.stream_id,
                    //     b.len()
                    // );
                    self.recv_buf = Cursor::new(Bytes::from(b));
                    self.recv_buf.read(buf)
                }
                Err(_) => Err(Error::from(ErrorKind::Other)),
            }
        }
    }
}
impl AsyncWrite for MuxStreamInner {
    fn shutdown(&mut self) -> Result<Async<()>, io::Error> {
        self.close();
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
                warn!("failed to acuire send window buf.");
                return Err(Error::from(ErrorKind::WouldBlock));
            }
            _ => {}
        }
        let mut send_len = buf.len();
        if send_len > self.state.send_buf_window.load(Ordering::SeqCst) as usize {
            send_len = self.state.send_buf_window.load(Ordering::SeqCst) as usize;
        }
        let ev = new_data_event(self.state.stream_id, &buf[0..send_len]);
        // info!(
        //     "[{}]new data ev with len:{} {}",
        //     self.state.stream_id,
        //     ev.header.len(),
        //     ev.body.len()
        // );
        let r = self.send_channel.start_send(ev);
        match r {
            Ok(AsyncSink::Ready) => {
                self.state
                    .send_buf_window
                    .fetch_sub(send_len as u32, Ordering::SeqCst);
                if self.state.send_buf_window.load(Ordering::SeqCst) > 0 {
                    send_permit.release(&self.state.window_sem);
                }
                self.send_channel.poll_complete();
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
    send_channel: UnboundedSender<Event>,
    recv_data_channel: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

impl ChannelMuxStream {
    pub fn new(id: u32, s: &UnboundedSender<Event>) -> Self {
        let state = MuxStreamState {
            stream_id: id,
            send_buf_window: AtomicU32::new(512 * 1024),
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
        let (send, recv) = mpsc::unbounded_channel();
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
    fn close(&mut self) {
        if !self.state.closed.load(Ordering::SeqCst) {
            self.state.closed.store(true, Ordering::SeqCst);
            self.write_event(new_fin_event(self.state.stream_id));
            if let Some(s) = &mut self.recv_data_channel {
                s.close();
            }
        }
    }
    fn handle_recv_data(&mut self, data: Vec<u8>) {
        let data_len = data.len() as u32;
        match &mut self.recv_data_channel {
            Some(tx) => {
                //info!("handle recv data with len:{}", data.len());
                tx.start_send(data);
                tx.poll_complete();
            }
            None => {
                error!("No stream data sender for {}", self.id());
                return;
            }
        }
        //info!("[{}]Recv data with len:{}", self.state.stream_id, data_len);
        let recv_window_size = self
            .state
            .recv_buf_window
            .fetch_add(data_len, Ordering::SeqCst);
        if recv_window_size >= 32 * 1024 {
            let ev = new_window_update_event(self.state.stream_id, recv_window_size);
            self.write_event(ev);
            //self.send_channel.poll_complete();
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
    fn write_event(&mut self, ev: Event) {
        self.send_channel.start_send(ev);
    }
}

pub struct ChannelMuxSession {
    event_trigger_send: UnboundedSender<Event>,
    streams: HashMap<u32, ChannelMuxStream>,
    next_stream_id: AtomicU32,
    //is_client: bool,
}

impl ChannelMuxSession {
    pub fn new(send: &UnboundedSender<Event>, is_client: bool) -> Self {
        let mut seed = AtomicU32::new(1);
        if !is_client {
            seed = AtomicU32::new(2);
        }
        Self {
            event_trigger_send: send.clone(),
            streams: HashMap::new(),
            next_stream_id: seed,
        }
    }
}

impl MuxSession for ChannelMuxSession {
    fn num_of_streams(&self) -> usize {
        self.streams.len()
    }
    fn close(&mut self) {
        self.event_trigger_send.start_send(new_shutdown_event(0));
        self.event_trigger_send.poll_complete();
    }
    fn ping(&mut self) {
        //info!("Send ping.");
        self.event_trigger_send.start_send(new_ping_event(0));
    }

    fn new_stream(&mut self, sid: u32) -> &mut dyn MuxStream {
        info!(
            "new stream:{} with alive streams:{}",
            sid,
            self.streams.len()
        );
        let s = ChannelMuxStream::new(sid, &self.event_trigger_send);
        self.streams.entry(sid).or_insert(s)
    }
    fn get_stream(&mut self, sid: u32) -> Option<&mut dyn MuxStream> {
        match self.streams.get_mut(&sid) {
            Some(s) => Some(s),
            None => None,
        }
    }
    fn next_stream_id(&mut self) -> u32 {
        self.next_stream_id.fetch_add(2, Ordering::SeqCst)
    }
    fn close_stream(&mut self, sid: u32, initial: bool) {
        let s = self.streams.remove(&sid);
        if initial {
            match s {
                Some(mut stream) => {
                    stream.close();
                }
                None => {}
            }
        }
    }
}
