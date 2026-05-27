use crate::mux::stream::{MuxStream, StreamFlow};
use anyhow::{anyhow, Result};
use bytes::Bytes;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
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

/// How long a single ping waits for its matching pong before declaring the
/// connection unhealthy. The health-check loop in `tunnel/client.rs` fires
/// every 1 second; a 2-second timeout gives one missed RTT of headroom while
/// still surfacing a half-open link within ~3 seconds of a stall. Tune higher
/// for very high-latency links (mobile/satellite); lower values risk
/// false-positive reconnects under transient load.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Per-stream state owned exclusively by the dispatcher.
struct StreamEntry {
    sender: mpsc::UnboundedSender<Option<Bytes>>,
    recv_window: u32,
    flow: Arc<StreamFlow>,
    /// Bytes pushed into unbounded channel but not yet consumed by MuxStream.
    /// Incremented on sender.send(), decremented when WindowUpdateToPeer fires
    /// (which means MuxStream read data and released window).
    pending_bytes: u64,
}

pub struct Connection {
    ev_writer: mpsc::Sender<Control>,
    stream_id_seed: AtomicU32,
    initial_stream_window: u32,
    pong_rx: mpsc::Receiver<u32>,
    ping_nonce_seed: AtomicU32,
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
        let (pong_sender, pong_rx) = mpsc::channel::<u32>(1);
        tokio::spawn(async move {
            handle_mux_connection(id, r, w, receiver, sender, stream_window, pong_sender).await;
        });
        match mode {
            Mode::Client => Self {
                ev_writer: sender_orig,
                stream_id_seed: AtomicU32::new(0),
                initial_stream_window: stream_window,
                pong_rx,
                ping_nonce_seed: AtomicU32::new(1),
            },
            Mode::Server => Self {
                ev_writer: sender_orig,
                stream_id_seed: AtomicU32::new(1),
                initial_stream_window: stream_window,
                pong_rx,
                ping_nonce_seed: AtomicU32::new(1),
            },
        }
    }

    pub async fn ping(&mut self) -> Result<()> {
        // Allocate a fresh monotonic nonce for this round-trip. Stale pongs
        // from previously timed-out pings carry old nonces and will be ignored
        // below — we drain them first as an optimization.
        while self.pong_rx.try_recv().is_ok() {}
        let nonce = self.ping_nonce_seed.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = self.ev_writer.send(Control::Ping(nonce)).await {
            return Err(anyhow::Error::new(e));
        }
        let deadline = tokio::time::Instant::now() + PING_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, self.pong_rx.recv()).await {
                Ok(Some(n)) if n == nonce => return Ok(()),
                Ok(Some(_)) => continue, // stale pong from a previous round; keep waiting
                Ok(None) => return Err(anyhow!("mux task terminated; connection unhealthy")),
                Err(_) => return Err(anyhow!("ping timeout after {:?}", PING_TIMEOUT)),
            }
        }
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

    /// Signal the spawned mux task to exit promptly.
    ///
    /// Without this, dropping `Connection` leaves the task running until the
    /// underlying transport errors out — which can take many seconds on a
    /// half-open TCP/TLS link, exactly the case where ping detection is most
    /// useful. Best-effort: if the channel is already closed, the task has
    /// already exited.
    pub fn close(&self) {
        let _ = self.ev_writer.try_send(Control::Close);
    }
}

async fn handle_mux_connection<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    conn_id: u32,
    r: R,
    mut w: W,
    mut ev_reader: mpsc::Receiver<Control>,
    ev_writer_orig: mpsc::Sender<Control>,
    initial_stream_window: u32,
    pong_sender: mpsc::Sender<u32>,
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
                event::FLAG_PING => {
                    if ev.body.len() == 4 {
                        let nonce = u32::from_le_bytes(ev.body[..4].try_into().unwrap());
                        Control::Pong(nonce)
                    } else {
                        // Older peer that didn't carry a nonce — echo zero so
                        // they at least see liveness.
                        Control::Pong(0)
                    }
                }
                event::FLAG_PONG => {
                    if ev.body.len() == 4 {
                        let nonce = u32::from_le_bytes(ev.body[..4].try_into().unwrap());
                        // Use try_send: at-most-one notification is sufficient, and an
                        // .await here would deadlock the read loop if the channel is
                        // already full (e.g., server side never drains, or peer sends
                        // duplicate pongs). Stale entries are filtered by nonce match
                        // in `ping()`.
                        let _ = pong_sender.try_send(nonce);
                    }
                    continue;
                }
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
                            pending_bytes: 0,
                        });
                        metrics::gauge!("mux.streams").increment(1.0);
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
                                metrics::gauge!("mux.streams").decrement(1.0);
                                let ev = event::new_fin_event(sid);
                                let _ = event::write_event(&mut w, ev).await;
                            } else {
                                entry.recv_window -= data_len;
                                entry.pending_bytes += data_len as u64;
                                if entry.sender.send(Some(data)).is_err() {
                                    tracing::error!(
                                        "[{}/{}] stream receiver dropped",
                                        conn_id,
                                        sid
                                    );
                                    entry.flow.close();
                                    stream_entries.remove(&sid);
                                    metrics::gauge!("mux.streams").decrement(1.0);
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
                        entry.pending_bytes = entry.pending_bytes.saturating_sub(increment as u64);
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
                        metrics::gauge!("mux.streams").decrement(1.0);
                        entry.flow.close();
                        if !remote {
                            let ev = event::new_fin_event(sid);
                            let _ = event::write_event(&mut w, ev).await;
                        } else {
                            let _ = entry.sender.send(None);
                        }
                    }
                }
                Control::Ping(nonce) => {
                    let ev = event::new_ping_event(nonce);
                    if let Err(e) = event::write_event(&mut w, ev).await {
                        tracing::error!("write ping failed:{}", e);
                        break;
                    }
                }
                Control::Pong(nonce) => {
                    let ev = event::new_pong_event(nonce);
                    if let Err(e) = event::write_event(&mut w, ev).await {
                        tracing::error!("write pong failed:{}", e);
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

            // Aggregate flow control and channel depth metrics across all streams.
            let mut total_recv_window: u64 = 0;
            let mut total_send_window: u64 = 0;
            let mut total_pending_bytes: u64 = 0;
            for entry in stream_entries.values() {
                total_recv_window += entry.recv_window as u64;
                total_send_window += entry.flow.available() as u64;
                total_pending_bytes += entry.pending_bytes;
            }
            metrics::gauge!("mux.stream.total_recv_window").set(total_recv_window as f64);
            metrics::gauge!("mux.stream.total_send_window").set(total_send_window as f64);
            metrics::gauge!("mux.stream.total_pending_bytes").set(total_pending_bytes as f64);
        }

        metrics::gauge!("mux.streams").decrement(stream_entries.len() as f64);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Two Connections wired back-to-back over `tokio::io::duplex` should
    /// successfully complete a ping round-trip: the client sends FLAG_PING,
    /// the server's read loop converts it to Control::Pong and writes
    /// FLAG_PONG, the client matches the nonce and returns Ok.
    #[tokio::test]
    async fn ping_roundtrip_succeeds() {
        let (a, b) = tokio::io::duplex(8192);
        let (a_r, a_w) = tokio::io::split(a);
        let (b_r, b_w) = tokio::io::split(b);

        let mut client =
            Connection::new_with_stream_window(a_r, a_w, Mode::Client, 0, INITIAL_STREAM_WINDOW);
        let _server =
            Connection::new_with_stream_window(b_r, b_w, Mode::Server, 1, INITIAL_STREAM_WINDOW);

        client.ping().await.expect("ping should succeed");
        // Second ping uses a fresh nonce — verifies the counter advances and
        // the previous round didn't poison state.
        client.ping().await.expect("second ping should succeed");
    }

    /// If the peer never echoes pongs (simulated by routing reads to a sink
    /// that never produces FLAG_PONG), `ping()` must time out within roughly
    /// `PING_TIMEOUT` rather than hang.
    #[tokio::test]
    async fn ping_times_out_when_peer_silent() {
        // duplex with a peer that we never service — writes will buffer up to
        // capacity but no one reads or replies.
        let (a, _b) = tokio::io::duplex(8192);
        let (a_r, a_w) = tokio::io::split(a);

        let mut client =
            Connection::new_with_stream_window(a_r, a_w, Mode::Client, 0, INITIAL_STREAM_WINDOW);

        let start = tokio::time::Instant::now();
        let err = client.ping().await.expect_err("ping should time out");
        let elapsed = start.elapsed();
        assert!(
            err.to_string().contains("timeout"),
            "expected timeout error, got: {}",
            err
        );
        // Allow generous slack for CI scheduling, but ensure we didn't return
        // immediately (which would indicate the timeout wasn't actually waited).
        assert!(
            elapsed >= PING_TIMEOUT.saturating_sub(Duration::from_millis(100)),
            "returned too early: {:?}",
            elapsed
        );
        assert!(
            elapsed < PING_TIMEOUT + Duration::from_secs(1),
            "returned too late: {:?}",
            elapsed
        );
    }
}
