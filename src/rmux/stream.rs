use super::event::{new_data_event, new_fin_event, Event};
use super::message::ConnectRequest;
use super::session::report_update_window;

use bytes::BytesMut;
use std::error::Error;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};
use std::time::Instant;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;

use crate::channel::ChannelStream;
use crate::utils::{fill_read_buf, make_io_error};

pub struct MuxStreamState {
    pub channel: String,
    pub session_id: u32,
    pub stream_id: u32,
    pub send_buf_window: AtomicI32,
    pub recv_buf_size: AtomicI32,
    pub closed: AtomicBool,
    pub total_recv_bytes: AtomicU32,
    pub total_send_bytes: AtomicU32,
    pub born_time: Instant,
}

struct SharedIOState {
    waker: Option<Waker>,
    data_tx: Option<mpsc::Sender<Vec<u8>>>,
    data_rx: Option<mpsc::Receiver<Vec<u8>>>,
}

impl SharedIOState {
    fn try_close(&mut self) {
        if let Some(tx) = &mut self.data_tx {
            let empty = Vec::new();
            let _ = tx.clone().try_send(empty);
        }
        if let Some(waker) = self.waker.take() {
            waker.wake()
        }
    }
}

impl MuxStreamState {
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

struct MuxStreamReader {
    rx: mpsc::Receiver<Vec<u8>>,
    recv_buf: BytesMut,
    state: Arc<MuxStreamState>,
}

impl MuxStreamReader {}

fn inc_recv_buf_window(state: &MuxStreamState, inc: usize, cx: &mut Context<'_>) {
    state.recv_buf_size.fetch_add(inc as i32, Ordering::SeqCst);
    state
        .total_recv_bytes
        .fetch_add(inc as u32, Ordering::SeqCst);
    let min_report_window: i32 = 32 * 1024;
    let current_recv_buf_size = state.recv_buf_size.load(Ordering::SeqCst);
    if current_recv_buf_size >= min_report_window
        && report_update_window(
            cx,
            state.channel.as_str(),
            state.session_id,
            state.stream_id,
            current_recv_buf_size as u32,
        )
    {
        state
            .recv_buf_size
            .fetch_sub(current_recv_buf_size, Ordering::SeqCst);
    }
}

impl AsyncRead for MuxStreamReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let Self {
            rx,
            recv_buf,
            state,
        } = &mut *self;
        if state.closed.load(Ordering::SeqCst) {
            rx.close();
            return Poll::Ready(Err(make_io_error("closed")));
        }
        if !recv_buf.is_empty() {
            let n = fill_read_buf(recv_buf, buf);
            inc_recv_buf_window(&state, n, cx);
            return Poll::Ready(Ok(n));
        }
        recv_buf.clear();
        match rx.poll_recv(cx) {
            Poll::Ready(Some(b)) => {
                let mut copy_n = b.len();
                if 0 == copy_n {
                    //close
                    //error!("[{}]####2 Close", state.stream_id);
                    state.close();
                    rx.close();
                    return Poll::Ready(Ok(0));
                }
                if copy_n > buf.len() {
                    copy_n = buf.len();
                }
                //info!("[{}]tx ready {} ", state.stream_id, copy_n);
                buf[0..copy_n].copy_from_slice(&b[0..copy_n]);
                if copy_n < b.len() {
                    recv_buf.extend_from_slice(&b[copy_n..]);
                }
                inc_recv_buf_window(&state, copy_n, cx);
                Poll::Ready(Ok(copy_n))
            }
            Poll::Ready(None) => {
                //error!("[{}]####3 Close", state.stream_id);
                state.close();
                rx.close();
                Poll::Ready(Ok(0))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

struct MuxStreamWriter {
    tx: mpsc::Sender<Event>,
    state: Arc<MuxStreamState>,
    io_state: Arc<Mutex<SharedIOState>>,
}
impl AsyncWrite for MuxStreamWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let Self {
            tx,
            state,
            io_state,
        } = &mut *self;
        if state.closed.load(Ordering::SeqCst) {
            io_state.lock().unwrap().try_close();
            return Poll::Ready(Err(make_io_error("closed")));
        }
        if state.send_buf_window.load(Ordering::SeqCst) < 0 {
            io_state.lock().unwrap().waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let ev = new_data_event(state.stream_id, buf, false);

        // let future = tx.send(ev);
        // pin_mut!(future);
        // match future.as_mut().poll(cx) {
        //     Poll::Pending => Poll::Pending,
        //     Poll::Ready(Err(e)) => Poll::Ready(Err(make_io_error(e.description()))),
        //     Poll::Ready(Ok(())) => {
        //         state
        //             .send_buf_window
        //             .fetch_sub(buf.len() as i32, Ordering::SeqCst);
        //         state
        //             .total_send_bytes
        //             .fetch_add(buf.len() as u32, Ordering::SeqCst);
        //         Poll::Ready(Ok(buf.len()))
        //     }
        // }
        match tx.poll_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => {
                io_state.lock().unwrap().try_close();
                return Poll::Ready(Err(make_io_error(e.description())));
            }
            Poll::Ready(Ok(())) => {}
        }
        match tx.try_send(ev) {
            Err(e) => {
                io_state.lock().unwrap().try_close();
                return Poll::Ready(Err(make_io_error(e.description())));
            }
            Ok(()) => {
                state
                    .send_buf_window
                    .fetch_sub(buf.len() as i32, Ordering::SeqCst);
                state
                    .total_send_bytes
                    .fetch_add(buf.len() as u32, Ordering::SeqCst);
                Poll::Ready(Ok(buf.len()))
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.state.closed.store(true, Ordering::SeqCst);
        self.io_state.lock().unwrap().try_close();
        Poll::Ready(Ok(()))
    }
}

pub struct MuxStream {
    pub target: ConnectRequest,
    event_tx: mpsc::Sender<Event>,
    pub data_tx: Option<mpsc::Sender<Vec<u8>>>,
    pub state: Arc<MuxStreamState>,
    io_state: Arc<Mutex<SharedIOState>>,
    relay_buf_size: usize,
}

impl MuxStream {
    pub fn new(
        name: &str,
        id0: u32,
        id1: u32,
        evtx: mpsc::Sender<Event>,
        target: ConnectRequest,
        relay_buf_size: usize,
    ) -> Self {
        let state = MuxStreamState {
            channel: String::from(name),
            session_id: id0,
            stream_id: id1,
            send_buf_window: AtomicI32::new(relay_buf_size as i32 * 4),
            recv_buf_size: AtomicI32::new(0),
            closed: AtomicBool::new(false),
            total_recv_bytes: AtomicU32::new(0),
            total_send_bytes: AtomicU32::new(0),
            born_time: Instant::now(),
        };
        let (dtx, drx) = mpsc::channel(8);
        let io_state = SharedIOState {
            waker: None,
            data_tx: Some(dtx),
            data_rx: Some(drx),
        };
        Self {
            target,
            event_tx: evtx,
            data_tx: None,
            state: Arc::new(state),
            io_state: Arc::new(Mutex::new(io_state)),
            relay_buf_size,
        }
    }
    pub fn id(&self) -> u32 {
        self.state.stream_id
    }

    pub fn relay_buf_size(&self) -> usize {
        self.relay_buf_size
    }

    fn check_data_tx(&mut self) {
        if self.data_tx.is_some() {
            return;
        }
        if let Some(tx) = self.io_state.lock().unwrap().data_tx.take() {
            self.data_tx = Some(tx);
        }
    }
    pub fn update_send_window(&self, inc: u32) {
        self.state
            .send_buf_window
            .fetch_add(inc as i32, Ordering::SeqCst);
        if self.state.send_buf_window.load(Ordering::SeqCst) > 0 {
            if let Some(waker) = self.io_state.lock().unwrap().waker.take() {
                waker.wake()
            }
        }
    }
    pub async fn offer_data(&mut self, data: Vec<u8>) {
        self.check_data_tx();
        if self.state.closed.load(Ordering::SeqCst) {
            error!(
                "[{}]Already closed for data len:{}.",
                self.state.stream_id,
                data.len()
            );
            return;
        }
        //error!("[{}]off data len:{}.", self.state.stream_id, data.len());
        assert!(!data.is_empty());
        if let Some(tx) = &mut self.data_tx {
            let _ = tx.send(data).await;
        } else {
            //error!("[{}]Non recv rx for data.", self.state.stream_id);
        }
    }
    pub fn clone(&self) -> Self {
        let mut v = Self {
            target: self.target.clone(),
            event_tx: self.event_tx.clone(),
            data_tx: None,
            state: self.state.clone(),
            io_state: self.io_state.clone(),
            relay_buf_size: self.relay_buf_size,
        };
        if let Some(tx) = &self.data_tx {
            v.data_tx = Some(tx.clone());
        }
        v
    }
}

impl ChannelStream for MuxStream {
    fn split(
        &mut self,
    ) -> (
        Box<dyn AsyncRead + Send + Unpin + '_>,
        Box<dyn AsyncWrite + Send + Unpin + '_>,
    ) {
        //let (dtx, drx) = mpsc::channel(16);
        let r = MuxStreamReader {
            rx: self.io_state.lock().unwrap().data_rx.take().unwrap(),
            recv_buf: BytesMut::new(),
            state: self.state.clone(),
        };
        let w = MuxStreamWriter {
            tx: self.event_tx.clone(),
            state: self.state.clone(),
            io_state: self.io_state.clone(),
        };
        //error!("[{}]split.", self.state.stream_id);
        (Box::new(r), Box::new(w))
    }
    fn close(&mut self) -> std::io::Result<()> {
        //error!("[{}]####1 Close", self.state.stream_id);
        self.state.close();
        if let Some(tx) = &self.data_tx {
            let empty = Vec::new();
            let _ = tx.clone().try_send(empty);
        }
        self.io_state.lock().unwrap().try_close();
        let fin = new_fin_event(self.state.stream_id, false);
        let _ = self.event_tx.try_send(fin);
        Ok(())
    }
}
