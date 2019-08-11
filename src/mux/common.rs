use super::event::*;
use super::multiplex::*;

use std::io::{Cursor, Error, ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use std::collections::HashMap;

use bytes::Bytes;

use tokio::io;
use tokio::io::{AsyncRead, AsyncWrite};

use tokio::prelude::*;
use tokio::sync::mpsc;
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
    recv_data_channel: mpsc::Receiver<Vec<u8>>,
    window_permit: Permit,
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
            self.write_event(new_fin_event(self.state.stream_id));
            //self.state.window_sem.close();
        }
    }

    fn write_event(&mut self, ev: Event) -> io::Result<()> {
        match self.send_channel.start_send(ev) {
            Ok(AsyncSink::Ready) => {
                self.send_channel.poll_complete();
                Ok(())
            }
            Ok(AsyncSink::NotReady(_)) => {
                warn!("[{}]Not ready to write local event.", self.state.stream_id);
                self.send_channel.poll_complete();
                Err(Error::from(ErrorKind::WouldBlock))
            }
            Err(_) => Err(Error::from(ErrorKind::Other)),
        }
    }

    fn inc_recv_window(&mut self, data_len: usize) {
        let n = data_len as u32;
        let recv_window_size = self.state.recv_buf_window.fetch_add(n, Ordering::SeqCst);
        if recv_window_size >= 32 * 1024 {
            let ev = new_window_update_event(self.state.stream_id, recv_window_size);
            if self.write_event(ev).is_ok() {
                self.state
                    .recv_buf_window
                    .fetch_sub(recv_window_size, Ordering::SeqCst);
            }
        }
    }
}

impl Drop for MuxStreamInner {
    fn drop(&mut self) {
        //info!("Dropping stream:{}", self.state.stream_id);
        self.write_event(new_fin_event(self.state.stream_id));
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
            self.inc_recv_window(n);
            return Ok(n);
        } else {
            let r = self.recv_data_channel.poll();
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
                    let n = self.recv_buf.read(buf)?;
                    self.inc_recv_window(n);
                    Ok(n)
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
        // if self.state.send_buf_window.load(Ordering::SeqCst) == 0 {
        //     warn!(
        //         "[{}]failed to acuire send window buf:{}",
        //         self.state.stream_id,
        //         self.state.send_buf_window.load(Ordering::SeqCst),
        //         //self.state.window_sem.available_permits()
        //     );
        //     return Err(Error::from(ErrorKind::WouldBlock));
        // }
        // let mut send_permit = Permit::new();
        let acuire = self.window_permit.poll_acquire(&self.state.window_sem);
        match acuire {
            Err(e) => {
                error!("failed to acuire:{}", e);
                return Err(Error::from(ErrorKind::Other));
            }
            Ok(Async::NotReady) => {
                // warn!(
                //     "[{}]failed to acuire send window buf:{} {}",
                //     self.state.stream_id,
                //     self.state.send_buf_window.load(Ordering::SeqCst),
                //     self.state.window_sem.available_permits()
                // );
                return Err(Error::from(ErrorKind::WouldBlock));
            }
            _ => {}
        }
        let mut send_len = buf.len();
        let blen = self.state.send_buf_window.load(Ordering::SeqCst) as usize;
        if send_len > blen {
            send_len = blen;
        }
        if 0 == send_len {
            warn!(
                "[{}]zero send window buf:{} {}",
                self.state.stream_id,
                self.state.send_buf_window.load(Ordering::SeqCst),
                self.state.window_sem.available_permits()
            );
            self.window_permit.forget();
            return Err(Error::from(ErrorKind::WouldBlock));
        }
        let ev = new_data_event(self.state.stream_id, &buf[0..send_len]);
        // info!(
        //     "[{}]new data ev with len:{} {}",
        //     self.state.stream_id,
        //     ev.header.len(),
        //     ev.body.len()
        // );

        let r = self.write_event(ev);
        match r {
            Ok(()) => {
                let slen = send_len as u32;
                if self.state.send_buf_window.fetch_sub(slen, Ordering::SeqCst) > slen {
                    self.window_permit.release(&self.state.window_sem);
                } else {
                    self.window_permit.forget();
                }
                Ok(send_len)
            }
            Err(e) => Err(e),
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
    send_data_channel: Option<mpsc::Sender<Vec<u8>>>,
}

impl ChannelMuxStream {
    pub fn new(id: u32, s: &mpsc::Sender<Event>) -> Self {
        let state = MuxStreamState {
            stream_id: id,
            send_buf_window: AtomicU32::new(128 * 1024),
            recv_buf_window: AtomicU32::new(0),
            window_sem: Semaphore::new(1),
            closed: AtomicBool::new(false),
        };
        Self {
            state: Arc::new(state),
            send_channel: s.clone(),
            send_data_channel: None,
        }
    }
}

impl MuxStream for ChannelMuxStream {
    fn split(&mut self) -> (Box<dyn AsyncRead + Send>, Box<dyn AsyncWrite + Send>) {
        let (send, recv) = mpsc::channel(128);
        self.send_data_channel = Some(send);

        let inner = MuxStreamInner {
            state: self.state.clone(),
            recv_buf: Cursor::new(Bytes::with_capacity(0)),
            send_channel: self.send_channel.clone(),
            recv_data_channel: recv,
            window_permit: Permit::new(),
        };
        let (r, w) = inner.split();
        (Box::new(r), Box::new(w))
    }
    fn close(&mut self, initial: bool) {
        if !self.state.closed.load(Ordering::SeqCst) {
            self.state.closed.store(true, Ordering::SeqCst);
            if initial {
                self.write_event(new_fin_event(self.state.stream_id));
            }
            if let Some(s) = &mut self.send_data_channel {
                s.close();
            }
        }
    }
    fn handle_recv_data(&mut self, data: Vec<u8>) {
        //let data_len = data.len() as u32;
        let sid = self.id();
        match &mut self.send_data_channel {
            Some(tx) => {
                //info!("handle recv data with len:{}", data.len());
                match tx.start_send(data) {
                    Ok(AsyncSink::Ready) => {
                        //
                        tx.poll_complete();
                    }
                    Ok(AsyncSink::NotReady(_)) => {
                        warn!("[{}]Not ready to handle recv data", sid);
                        tx.poll_complete();
                    }
                    Err(e) => {
                        error!("[{}]Failed to handle recv data:{}", sid, e);
                    }
                }
            }
            None => {
                error!("No stream data sender for {}", sid);
                return;
            }
        }
    }
    fn handle_window_update(&mut self, len: u32) {
        self.state.send_buf_window.fetch_add(len, Ordering::SeqCst);
        //info!("[{}]Add window:{} ", self.id(), len);
        if self.state.window_sem.available_permits() == 0 {
            self.state.window_sem.add_permits(1);
            // info!(
            //     "[{}]After add window:{} while permits:{}",
            //     self.id(),
            //     len,
            //     self.state.window_sem.available_permits()
            // );
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
    event_trigger_send: mpsc::Sender<Event>,
    streams: HashMap<u32, ChannelMuxStream>,
    next_stream_id: AtomicU32,
    id: u32,
}

impl ChannelMuxSession {
    pub fn new(send: &mpsc::Sender<Event>, is_client: bool, id: u32) -> Self {
        let mut seed = AtomicU32::new(1);
        if !is_client {
            seed = AtomicU32::new(2);
        }
        Self {
            event_trigger_send: send.clone(),
            streams: HashMap::new(),
            next_stream_id: seed,
            id,
        }
    }
}

impl MuxSession for ChannelMuxSession {
    fn id(&self) -> u32 {
        self.id
    }
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
            "[{}]new stream:{} with alive streams:{}",
            self.id(),
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
        info!(
            "[{}]close stream:{} initial:{} with alive streams:{}",
            self.id(),
            sid,
            initial,
            self.streams.len()
        );
        let s = self.streams.remove(&sid);
        match s {
            Some(mut stream) => {
                stream.close(initial);
            }
            None => {}
        }
    }
}
