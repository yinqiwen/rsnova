use crate::mux::stream::MuxStream;
use anyhow::{anyhow, Result};
use bytes::Bytes;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use super::event;

use super::stream::Control;

pub const DEFAULT_STREAM_CHANNEL_SIZE: usize = 16;

pub struct Connection {
    ev_writer: mpsc::Sender<Control>,
    stream_id_seed: AtomicU32,
    stream_channel_size: usize,
}

pub enum Mode {
    Server,
    Client,
}

impl Connection {
    pub fn new_with_stream_channel_size<
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    >(
        r: R,
        w: W,
        mode: Mode,
        id: u32,
        stream_channel_size: usize,
    ) -> Self {
        let (sender_orig, receiver) = mpsc::channel::<Control>(4096);
        let sender = sender_orig.clone();
        tokio::spawn(async move {
            handle_mux_connection(id, r, w, receiver, sender, stream_channel_size).await;
        });
        match mode {
            Mode::Client => Self {
                ev_writer: sender_orig,
                stream_id_seed: AtomicU32::new(0),
                stream_channel_size,
            },
            Mode::Server => Self {
                ev_writer: sender_orig,
                stream_id_seed: AtomicU32::new(1),
                stream_channel_size,
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
        let (sender, receiver) = mpsc::channel::<Option<Bytes>>(self.stream_channel_size);
        let id = self.stream_id_seed.fetch_add(2, Ordering::SeqCst);
        let stream = MuxStream::new(id, self.ev_writer.clone(), receiver);
        if let Err(e) = self
            .ev_writer
            .send(Control::NewStream((id, sender, None)))
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
    stream_channel_size: usize,
) {
    let ev_writer = ev_writer_orig.clone();
    let read_connection_fut = async move {
        let mut buf_reader = tokio::io::BufReader::new(r);
        while let Ok(ev) = event::read_event(&mut buf_reader).await {
            match ev.header.flags() {
                event::FLAG_SYN => {
                    let (sender, receiver) = mpsc::channel::<Option<Bytes>>(stream_channel_size);
                    let ctrl = Control::NewStream((ev.header.stream_id, sender, Some(receiver)));
                    if ev_writer.send(ctrl).await.is_err() {
                        break;
                    }
                }
                event::FLAG_FIN => {
                    if ev_writer
                        .send(Control::StreamClose(ev.header.stream_id, true))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                event::FLAG_SHUTDOWN => {
                    if ev_writer
                        .send(Control::StreamShutdown(ev.header.stream_id, true))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                event::FLAG_DATA => {
                    if ev_writer
                        .send(Control::StreamData(ev.header.stream_id, ev.body, true))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                event::FLAG_PING => {
                    //
                }
                _ => {
                    tracing::error!(
                        "Unexpected event:{}/{}",
                        ev.header.flags(),
                        ev.header.stream_id
                    );
                }
            }
        }
        let _ = ev_writer.send(Control::Close).await;
    };

    let ev_writer = ev_writer_orig.clone();
    let read_ctrl_fut = async move {
        //let labels: [(&str, String); 1] = [("idx", format!("{}", conn_id))];
        let mut incoming_streams: VecDeque<MuxStream> = VecDeque::new();
        let mut accept_callback: Option<oneshot::Sender<Result<MuxStream>>> = None;
        let mut stream_senders: HashMap<u32, mpsc::Sender<Option<Bytes>>> = HashMap::new();

        while let Some(ctrl) = ev_reader.recv().await {
            match ctrl {
                Control::AcceptStream(callback) => {
                    if accept_callback.is_some() {
                        let _ = callback.send(Err(anyhow!("duplicate accept")));
                        continue;
                    }
                    accept_callback = Some(callback);
                }
                Control::NewStream((sid, sender, receiver)) => match stream_senders.entry(sid) {
                    Entry::Occupied(e) => {
                        tracing::error!("Duplicate stream id:{}", sid);
                        let _ = e.get().send(Some(Bytes::new())).await;
                    }
                    Entry::Vacant(v) => {
                        v.insert(sender);
                        metrics::increment_gauge!("mux.streams", 1.0);
                        if receiver.is_some() {
                            let stream = MuxStream::new(sid, ev_writer.clone(), receiver.unwrap());
                            incoming_streams.push_back(stream);
                        } else {
                            let ev = event::new_syn_event(sid);
                            if let Err(e) = event::write_event(&mut w, ev).await {
                                tracing::error!("write syn failed:{}", e);
                                break;
                            }
                        }
                    }
                },
                Control::StreamData(sid, data, incoming) => {
                    match stream_senders.get(&sid) {
                        Some(stream_sender) => {
                            if incoming {
                                let data_len = data.len();
                                if let Err(e) = stream_sender.send(Some(data)).await {
                                    // handle error
                                    tracing::error!(
                                        "Stream:{}/{} send error:{} with data len:{}",
                                        conn_id,
                                        sid,
                                        e,
                                        data_len
                                    );
                                }
                            } else {
                                let ev = event::new_data_event(sid, data);
                                if let Err(e) = event::write_event(&mut w, ev).await {
                                    tracing::error!("write stream data failed:{}", e);
                                    break;
                                }
                            }
                        }
                        None => {
                            if !data.is_empty() {
                                tracing::error!(
                                    "No stream:{}/{} found for for data incoming:{} with data len:{}",
                                    conn_id,sid,
                                    incoming,
                                    data.len()
                                );
                            }
                        }
                    }
                }
                Control::StreamShutdown(sid, remote) => {
                    tracing::info!(
                        "[{}/{}]Stream shutdown write from remote:{}",
                        conn_id,
                        sid,
                        remote
                    );
                    if let Some(sender) = stream_senders.get(&sid) {
                        if !remote {
                            let ev = event::new_shutdown_event(sid);
                            if let Err(e) = event::write_event(&mut w, ev).await {
                                tracing::error!("write fin failed:{}", e);
                                break;
                            }
                        } else {
                            let _ = sender.send(Some(Bytes::new())).await;
                        }
                    }
                }
                Control::StreamClose(sid, remote) => {
                    tracing::info!("[{}/{}]Stream close from remote:{}", conn_id, sid, remote);
                    match stream_senders.remove_entry(&sid) {
                        Some((_, sender)) => {
                            metrics::decrement_gauge!("mux.streams", 1.0);
                            if !remote {
                                let ev = event::new_fin_event(sid);
                                let _ = event::write_event(&mut w, ev).await;
                            } else {
                                let _ = sender.send(None).await;
                            }
                        }
                        None => {
                            //
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

        //close streams
        metrics::decrement_gauge!("mux.streams", stream_senders.len() as f64);
        for (_, sender) in stream_senders.drain() {
            let _ = sender.send(None).await;
        }
        if accept_callback.is_some() {
            let _ = accept_callback
                .unwrap()
                .send(Err(anyhow!("connection closed")));
            //accept_callback = None;
        }
    };
    tokio::join!(read_connection_fut, read_ctrl_fut);
}
