use crate::mux::event::*;
use crate::mux::manager::*;
use crate::mux::session::*;

use std::io;
use std::io::Error;
use std::io::ErrorKind;
use std::io::{Cursor, Read, Write};

use bytes::Bytes;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::prelude::*;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_sync::semaphore::{Permit, Semaphore};

use futures::sync::mpsc;

struct StreamState {
    closed: bool,
    recv_channel: mpsc::Receiver<Bytes>,
    recv_buf: Cursor<Bytes>,

    send_buf_window: u32,
    recv_buf_window: u32,
    window_sem: Semaphore,
}

pub struct CommonMuxStream<T: AsyncRead + AsyncWrite + Send + 'static> {
    pub sid: u32,
    //pub recv_buf: Arc<Mutex<VecDeque<Cursor<Bytes>>>>,
    pub conn: ConnRef<T>,
    state: Arc<Mutex<StreamState>>,
    send_channel: mpsc::Sender<Bytes>,
}

impl<T: AsyncRead + AsyncWrite + Send + 'static> CommonMuxStream<T> {
    pub fn new(id: u32, conn: &ConnRef<T>) -> Self {
        let (send, recv) = mpsc::channel(1024);
        let state = StreamState {
            closed: false,
            recv_channel: recv,
            recv_buf: Cursor::new(Bytes::with_capacity(0)),
            send_buf_window: 512 * 1024,
            recv_buf_window: 0,
            window_sem: Semaphore::new(1),
        };
        Self {
            sid: id,
            conn: conn.clone(),
            state: Arc::new(Mutex::new(state)),
            send_channel: send,
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Send + 'static> MuxStream for CommonMuxStream<T> {
    fn handle_recv_data(&mut self, data: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        let data_len = data.len();
        self.send_channel.clone().try_send(Bytes::from(data));
        state.recv_buf_window += data_len as u32;
        if state.recv_buf_window >= 32 * 1024 {
            let ev = new_window_update_event(self.sid, state.recv_buf_window);
            self.conn.lock().unwrap().write_event(ev);
            state.recv_buf_window = 0;
        }
        //state.recv_buf.push_back(Cursor::new(buf));
        //self.tx.send(buf);
    }

    fn handle_window_update(&mut self, len: u32) {
        let mut state = self.state.lock().unwrap();
        state.send_buf_window += len;
        if state.window_sem.available_permits() == 0 {
            state.window_sem.add_permits(1);
        }
    }

    fn split(&self) -> (Box<dyn AsyncRead + Send>, Box<dyn AsyncWrite + Send>) {
        let recv = RecvStream {
            //rbuf: self.staterecv_buf.clone(),
            state: self.state.clone(),
        };
        let send = SendStream {
            stream_id: self.sid,
            conn: self.conn.clone(),
            state: self.state.clone(),
        };
        (Box::new(recv), Box::new(send))
    }
    fn close(&mut self) {
        //self.state.closed = true;
        self.state.lock().unwrap().closed = true;
        self.conn.lock().unwrap().close_stream(self.sid, true);
    }
}

struct SendStream<T: AsyncRead + AsyncWrite + Send + 'static> {
    pub stream_id: u32,
    pub conn: ConnRef<T>,
    state: Arc<Mutex<StreamState>>,
}

struct RecvStream {
    //pub rbuf: Arc<Mutex<VecDeque<Cursor<Bytes>>>>,
    state: Arc<Mutex<StreamState>>,
}

impl AsyncRead for RecvStream {}
impl Read for RecvStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(Error::from(ErrorKind::ConnectionReset));
        }
        let n = state.recv_buf.read(buf)?;
        if n > 0 {
            return Ok(n);
        } else {
            let r = state.recv_channel.poll();
            match r {
                Ok(Async::NotReady) => Err(Error::from(ErrorKind::WouldBlock)),
                Ok(Async::Ready(None)) => Err(Error::from(ErrorKind::ConnectionReset)),
                Ok(Async::Ready(Some(b))) => {
                    state.recv_buf = Cursor::new(b);
                    state.recv_buf.read(buf)
                }
                Err(_) => Err(Error::from(ErrorKind::Other)),
            }
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Send + 'static> AsyncWrite for SendStream<T> {
    fn shutdown(&mut self) -> Result<Async<()>, io::Error> {
        self.state.lock().unwrap().closed = true;
        self.conn.lock().unwrap().close_stream(self.stream_id, true);
        Ok(Async::Ready(()))
    }
}

impl<T: AsyncRead + AsyncWrite + Send + 'static> Write for SendStream<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            //self.shutdown();
            return Err(Error::from(ErrorKind::ConnectionReset));
        }

        let mut send_permit = Permit::new();
        let acuire = send_permit.poll_acquire(&state.window_sem);
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
        if send_len > state.send_buf_window as usize {
            send_len = state.send_buf_window as usize;
        }
        let ev = new_data_event(self.stream_id, &buf[0..send_len]);
        let r = self.conn.lock().unwrap().write_event(ev)?;
        match r {
            AsyncSink::Ready => {
                state.send_buf_window -= send_len as u32;
                if state.send_buf_window > 0 {
                    send_permit.release(&state.window_sem);
                }
                Ok(send_len)
            }
            AsyncSink::NotReady(_) => Err(Error::from(ErrorKind::WouldBlock)),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.conn.lock().unwrap().flush();
        Ok(())
    }
}
