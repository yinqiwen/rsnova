use crate::mux::stream::{MuxStream, StreamFlow};
use anyhow::{anyhow, Result};
use bytes::Bytes;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use super::event;
use super::stream::{Control, NewStreamParams};

pub const INITIAL_STREAM_WINDOW: u32 = 256 * 1024;
/// Documentation only — actual threshold is computed per-MuxStream as initial_stream_window / 2.
#[allow(dead_code)]
pub const WINDOW_UPDATE_THRESHOLD: u32 = INITIAL_STREAM_WINDOW / 2;
pub const CONTROL_CHANNEL_CAPACITY: usize = 256;

/// Per-stream state owned exclusively by the dispatcher.
struct StreamEntry {
    sender: mpsc::UnboundedSender<Option<Bytes>>,
    recv_window: u32,
    flow: Arc<StreamFlow>,
}

pub struct Connection {
    ev_writer: mpsc::Sender<Control>,
    stream_id_seed: AtomicU32,
    initial_stream_window: u32,
}

pub enum Mode {
    Server,
    Client,
}

impl Connection {
    pub fn new_with_stream_window<
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    >(
        r: R,
        w: W,
        mode: Mode,
        id: u32,
        stream_window: u32,
    ) -> Self {
        let (sender_orig, receiver) = mpsc::channel::<Control>(CONTROL_CHANNEL_CAPACITY);
        let sender = sender_orig.clone();
        tokio::spawn(async move {
            handle_mux_connection(id, r, w, receiver, sender, stream_window).await;
        });
        match mode {
            Mode::Client => Self {
                ev_writer: sender_orig,
                stream_id_seed: AtomicU32::new(0),
                initial_stream_window: stream_window,
            },
            Mode::Server => Self {
                ev_writer: sender_orig,
                stream_id_seed: AtomicU32::new(1),
                initial_stream_window: stream_window,
            },
        }
    }

    pub async fn ping(&self) -> Result<()> {
        if let Err(e) = self.ev_writer.send(Control::Ping).await {
            return Err(anyhow::Error::new(e));
        }
        Ok(())
    }

    pub async fn open_stream(&self) -> Result<MuxStream> {
        let (sender, receiver) = mpsc::unbounded_channel::<Option<Bytes>>();
        let flow = Arc::new(StreamFlow::new(self.initial_stream_window));
        let id = self.stream_id_seed.fetch_add(2, Ordering::SeqCst);
        let stream = MuxStream::new(
            id,
            self.ev_writer.clone(),
            receiver,
            flow.clone(),
            self.initial_stream_window,
        );
        if let Err(e) = self
            .ev_writer
            .send(Control::NewStream(NewStreamParams {
                stream_id: id,
                sender,
                receiver: None,
                flow,
            }))
            .await
        {
            return Err(anyhow::Error::new(e));
        }
        Ok(stream)
    }

    pub async fn accept_stream(&self) -> Result<MuxStream> {
        let (sender, receiver) = oneshot::channel::<Result<MuxStream>>();
        if let Err(e) = self.ev_writer.send(Control::AcceptStream(sender)).await {
            return Err(anyhow::Error::new(e));
        }
        match receiver.await {
            Ok(v) => v,
            Err(e) => Err(anyhow::Error::new(e)),
        }
    }
}

async fn handle_mux_connection<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    conn_id: u32,
    r: R,
    mut w: W,
    mut ev_reader: mpsc::Receiver<Control>,
    ev_writer_orig: mpsc::Sender<Control>,
    initial_stream_window: u32,
) {
    let ev_writer = ev_writer_orig.clone();
    let read_connection_fut = async move {
        let mut buf_reader = tokio::io::BufReader::new(r);
        while let Ok(ev) = event::read_event(&mut buf_reader).await {
            let ctrl = match ev.header.flags() {
                event::FLAG_SYN => {
                    let (sender, receiver) = mpsc::unbounded_channel::<Option<Bytes>>();
                    let flow = Arc::new(StreamFlow::new(initial_stream_window));
                    Control::NewStream(NewStreamParams {
                        stream_id: ev.header.stream_id,
                        sender,
                        receiver: Some(receiver),
                        flow,
                    })
                }
                event::FLAG_FIN => Control::StreamClose(ev.header.stream_id, true),
                event::FLAG_SHUTDOWN => Control::StreamShutdown(ev.header.stream_id, true),
                event::FLAG_DATA => Control::StreamData(ev.header.stream_id, ev.body, true),
                event::FLAG_PING => continue,
                event::FLAG_WIN_UPDATE => {
                    if ev.body.len() == 4 {
                        let increment = u32::from_le_bytes(ev.body[..4].try_into().unwrap());
                        Control::WindowUpdateFromPeer(ev.header.stream_id, increment)
                    } else {
                        continue;
                    }
                }
                // FLAG_AUTH/AUTH_ACK/OPEN/REVERSE_OPEN payloads travel via FLAG_DATA at app layer.
                _ => {
                    tracing::error!(
                        "Unexpected event:{}/{}",
                        ev.header.flags(),
                        ev.header.stream_id
                    );
                    continue;
                }
            };
            if ev_writer.send(ctrl).await.is_err() {
                break;
            }
        }
        let _ = ev_writer.send(Control::Close).await;
    };

    let ev_writer = ev_writer_orig.clone();
    let read_ctrl_fut = async move {
        let mut incoming_streams: VecDeque<MuxStream> = VecDeque::new();
        let mut accept_callback: Option<oneshot::Sender<Result<MuxStream>>> = None;
        let mut stream_entries: HashMap<u32, StreamEntry> = HashMap::new();

        while let Some(ctrl) = ev_reader.recv().await {
            match ctrl {
                Control::AcceptStream(callback) => {
                    if accept_callback.is_some() {
                        let _ = callback.send(Err(anyhow!("duplicate accept")));
                        continue;
                    }
                    accept_callback = Some(callback);
                }
                Control::NewStream(params) => match stream_entries.entry(params.stream_id) {
                    Entry::Occupied(_) => {
                        tracing::error!("Duplicate stream id:{}", params.stream_id);
                    }
                    Entry::Vacant(v) => {
                        v.insert(StreamEntry {
                            sender: params.sender,
                            recv_window: initial_stream_window,
                            flow: params.flow.clone(),
                        });
                        metrics::increment_gauge!("mux.streams", 1.0);
                        if let Some(rx) = params.receiver {
                            let stream = MuxStream::new(
                                params.stream_id,
                                ev_writer.clone(),
                                rx,
                                params.flow,
                                initial_stream_window,
                            );
                            incoming_streams.push_back(stream);
                        } else {
                            let ev = event::new_syn_event(params.stream_id);
                            if let Err(e) = event::write_event(&mut w, ev).await {
                                tracing::error!("write syn failed:{}", e);
                                break;
                            }
                        }
                    }
                },
                Control::StreamData(sid, data, incoming) => {
                    if incoming {
                        if let Some(entry) = stream_entries.get_mut(&sid) {
                            let data_len = data.len() as u32;
                            if data_len > entry.recv_window {
                                tracing::error!(
                                    "[{}/{}] flow control violation: {} > recv_window {}",
                                    conn_id,
                                    sid,
                                    data_len,
                                    entry.recv_window
                                );
                                entry.flow.close();
                                let _ = entry.sender.send(None);
                                stream_entries.remove(&sid);
                                metrics::decrement_gauge!("mux.streams", 1.0);
                                let ev = event::new_fin_event(sid);
                                let _ = event::write_event(&mut w, ev).await;
                            } else {
                                entry.recv_window -= data_len;
                                if entry.sender.send(Some(data)).is_err() {
                                    tracing::error!(
                                        "[{}/{}] stream receiver dropped",
                                        conn_id,
                                        sid
                                    );
                                    entry.flow.close();
                                    stream_entries.remove(&sid);
                                    metrics::decrement_gauge!("mux.streams", 1.0);
                                }
                            }
                        }
                    } else {
                        let ev = event::new_data_event(sid, data);
                        if let Err(e) = event::write_event(&mut w, ev).await {
                            tracing::error!("write stream data failed:{}", e);
                            break;
                        }
                    }
                }
                Control::WindowUpdateFromPeer(sid, increment) => {
                    if let Some(entry) = stream_entries.get(&sid) {
                        entry.flow.credit(increment);
                    }
                }
                Control::WindowUpdateToPeer(sid, increment) => {
                    if let Some(entry) = stream_entries.get_mut(&sid) {
                        entry.recv_window = entry
                            .recv_window
                            .saturating_add(increment)
                            .min(initial_stream_window);
                        let ev = event::new_window_update_event(sid, increment);
                        if let Err(e) = event::write_event(&mut w, ev).await {
                            tracing::error!("write window update failed:{}", e);
                            break;
                        }
                    }
                }
                Control::StreamShutdown(sid, remote) => {
                    if let Some(entry) = stream_entries.get(&sid) {
                        if !remote {
                            let ev = event::new_shutdown_event(sid);
                            if let Err(e) = event::write_event(&mut w, ev).await {
                                tracing::error!("write shutdown failed:{}", e);
                                break;
                            }
                        } else {
                            let _ = entry.sender.send(Some(Bytes::new()));
                        }
                    }
                }
                Control::StreamClose(sid, remote) => {
                    if let Some(entry) = stream_entries.remove(&sid) {
                        metrics::decrement_gauge!("mux.streams", 1.0);
                        entry.flow.close();
                        if !remote {
                            let ev = event::new_fin_event(sid);
                            let _ = event::write_event(&mut w, ev).await;
                        } else {
                            let _ = entry.sender.send(None);
                        }
                    }
                }
                Control::Ping => {
                    let ev = event::new_ping_event();
                    if let Err(e) = event::write_event(&mut w, ev).await {
                        tracing::error!("write ping failed:{}", e);
                        break;
                    }
                }
                Control::Close => {
                    break;
                }
            }

            if accept_callback.is_some() && !incoming_streams.is_empty() {
                let stream = incoming_streams.pop_front().unwrap();
                let _ = accept_callback.unwrap().send(Ok(stream));
                accept_callback = None;
            }
        }

        metrics::decrement_gauge!("mux.streams", stream_entries.len() as f64);
        for (_, entry) in stream_entries.drain() {
            entry.flow.close();
            let _ = entry.sender.send(None);
        }
        if let Some(cb) = accept_callback {
            let _ = cb.send(Err(anyhow!("connection closed")));
        }
    };
    tokio::join!(read_connection_fut, read_ctrl_fut);
}
