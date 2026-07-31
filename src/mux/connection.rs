use crate::mux::stream::{MuxStream, StreamFlow};
use anyhow::{Result, anyhow};
use bytes::Bytes;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use super::event;
use super::stream::{Control, NewStreamParams};

pub const INITIAL_STREAM_WINDOW: u32 = 256 * 1024;
pub const MIN_STREAM_WINDOW: u32 = 16 * 1024;
pub const MAX_STREAM_WINDOW: u32 = 4 * 1024 * 1024;
/// Documentation only — actual threshold is computed per-MuxStream as initial_stream_window / 2.
#[allow(dead_code)]
pub const WINDOW_UPDATE_THRESHOLD: u32 = INITIAL_STREAM_WINDOW / 2;
pub const CONTROL_CHANNEL_CAPACITY: usize = 256;
pub const MAX_PENDING_INCOMING_STREAMS: usize = 1024;
pub const MAX_MUX_STREAMS_PER_CONNECTION: usize = 1024;
/// How many multiples of `initial_stream_window` a stream's dispatcher-side
/// backlog (`pending_bytes`: bytes pushed into the per-stream channel but not
/// yet consumed by the application) may reach before the stream is declared
/// unresponsive and forcibly closed.
///
/// The inbound data channel is *unbounded*, so without this cap a consumer
/// that stops reading (buggy upstream, dead task, slowloris-style peer) lets
/// `pending_bytes` grow without limit — one stuck stream per connection can
/// OOM the process. Normal backpressure keeps `pending_bytes` near
/// `initial_stream_window` (the peer pauses when its send window empties);
/// 4× headroom absorbs transient bursts without false positives. When the
/// cap trips we send FIN to the peer, drop the entry, and return the memory.
const PENDING_BYTES_CAP_WINDOW_MULTIPLE: u64 = 4;

fn incoming_stream_rejection_reason(
    pending_incoming_streams: usize,
    active_streams: usize,
) -> Option<&'static str> {
    if pending_incoming_streams >= MAX_PENDING_INCOMING_STREAMS {
        Some("pending")
    } else if active_streams >= MAX_MUX_STREAMS_PER_CONNECTION {
        Some("active")
    } else {
        None
    }
}

/// How long a single ping waits for its matching pong before declaring the
/// connection unhealthy. The health-check loop in `tunnel/client.rs` fires
/// every 1 second; 5 seconds absorbs a TCP retransmission (or two) on a lossy
/// path without a false-positive reconnect, while still surfacing a half-open
/// link within roughly one ping interval of a stall. Tune higher for very
/// high-latency links (mobile/satellite); lower values risk false-positive
/// reconnects under transient load.
const PING_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a ping round-trip failed.
///
/// The distinction drives connection lifecycle. A `Timeout` may be a transient
/// stall on a still-live link — a single lost packet plus TCP retransmission
/// backoff exceeds `PING_TIMEOUT` on a lossy path — so the caller applies its
/// consecutive-failure threshold before giving up on the connection. `Dead`
/// means the mux dispatcher is gone and no amount of retrying revives it.
#[derive(Debug)]
pub enum PingError {
    Timeout(Duration),
    Dead(&'static str),
}

impl PingError {
    /// `true` when the connection can never recover and must be replaced
    /// immediately, bypassing any consecutive-failure threshold.
    pub fn is_fatal(&self) -> bool {
        matches!(self, PingError::Dead(_))
    }
}

impl std::fmt::Display for PingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PingError::Timeout(d) => write!(f, "ping timeout after {:?}", d),
            PingError::Dead(reason) => write!(f, "{}", reason),
        }
    }
}

impl std::error::Error for PingError {}

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
    conn_id: u32,
    ev_writer: mpsc::Sender<Control>,
    stream_id_seed: AtomicU32,
    initial_stream_window: u32,
    pong_rx: mpsc::Receiver<u32>,
    ping_nonce_seed: AtomicU32,
    window_update_sender: mpsc::UnboundedSender<(u32, u32)>,
    /// Live mux streams on this connection. Read by `ping()` to implement
    /// active-traffic liveness (data flow proves the link is up), and by
    /// `tunnel_remote` to refuse selecting a dead handler for a new visitor.
    active_stream_count: Arc<AtomicUsize>,
    /// Handle to the spawned dispatcher task. Aborted on Drop as
    /// defense-in-depth: `Connection::close()` is best-effort
    /// (`try_send(Control::Close)`), which can silently fail if the channel
    /// is full or the dispatcher has already exited. Without abort, a
    /// half-open TCP link (peer never sends FIN) keeps the dispatcher's
    /// `read_connection_fut` blocked forever, leaking the socket FD.
    #[allow(dead_code)]
    dispatch_task: tokio::task::JoinHandle<()>,
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
        let (window_update_sender, window_update_receiver) =
            mpsc::unbounded_channel::<(u32, u32)>();
        let wus = window_update_sender.clone();
        let active_stream_count = Arc::new(AtomicUsize::new(0));
        let active_stream_count_for_dispatcher = active_stream_count.clone();
        let dispatch_task = tokio::spawn(async move {
            handle_mux_connection(
                id,
                r,
                w,
                receiver,
                sender,
                stream_window,
                pong_sender,
                wus,
                window_update_receiver,
                active_stream_count_for_dispatcher,
            )
            .await;
        });
        match mode {
            Mode::Client => Self {
                conn_id: id,
                ev_writer: sender_orig,
                stream_id_seed: AtomicU32::new(0),
                initial_stream_window: stream_window,
                pong_rx,
                ping_nonce_seed: AtomicU32::new(1),
                window_update_sender,
                active_stream_count,
                dispatch_task,
            },
            Mode::Server => Self {
                conn_id: id,
                ev_writer: sender_orig,
                stream_id_seed: AtomicU32::new(1),
                initial_stream_window: stream_window,
                pong_rx,
                ping_nonce_seed: AtomicU32::new(1),
                window_update_sender,
                active_stream_count,
                dispatch_task,
            },
        }
    }

    /// `true` when at least one mux stream is currently active on this
    /// connection. Data flowing through active streams is proof the link is
    /// up; `ping()` uses this to skip the wire round-trip, and tunnel code
    /// uses it to detect a dead-but-not-yet-torn-down connection before
    /// handing it a new visitor stream.
    pub fn has_active_streams(&self) -> bool {
        self.active_stream_count.load(Ordering::Relaxed) > 0
    }

    pub async fn ping(&mut self) -> std::result::Result<(), PingError> {
        // Active streams are already exchanging DATA frames with the peer in
        // both directions — that is stronger liveness evidence than a PING
        // round-trip. Skipping the wire ping avoids false timeouts when DATA
        // saturates the link (a PING queued behind a full window easily blows
        // the 5s budget on a healthy connection), and keeps reverse-tunnel
        // control connections alive while they carry visitor traffic.
        // Trade-off: an idle tunnel control connection whose peer
        // half-vanishes (NAT silently drops the session) is only detected
        // when a reverse stream actually fails; the reconnect loop then
        // recovers within one backoff interval.
        if self.has_active_streams() {
            return Ok(());
        }
        // Allocate a fresh monotonic nonce for this round-trip. Stale pongs
        // from previously timed-out pings carry old nonces and will be ignored
        // below — we drain them first as an optimization.
        while self.pong_rx.try_recv().is_ok() {}
        let nonce = self.ping_nonce_seed.fetch_add(1, Ordering::Relaxed);
        if self.ev_writer.send(Control::Ping(nonce)).await.is_err() {
            return Err(PingError::Dead("mux task terminated; ping not sent"));
        }
        let deadline = tokio::time::Instant::now() + PING_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, self.pong_rx.recv()).await {
                Ok(Some(n)) if n == nonce => return Ok(()),
                Ok(Some(_)) => continue, // stale pong from a previous round; keep waiting
                Ok(None) => {
                    return Err(PingError::Dead("mux task terminated; connection unhealthy"));
                }
                Err(_) => return Err(PingError::Timeout(PING_TIMEOUT)),
            }
        }
    }

    pub async fn open_stream(&self) -> Result<MuxStream> {
        let (sender, receiver) = mpsc::unbounded_channel::<Option<Bytes>>();
        let flow = Arc::new(StreamFlow::new(self.initial_stream_window));
        let id = self.stream_id_seed.fetch_add(2, Ordering::SeqCst);
        let stream = MuxStream::new(
            self.conn_id,
            id,
            self.ev_writer.clone(),
            receiver,
            flow.clone(),
            self.initial_stream_window,
            self.window_update_sender.clone(),
        );
        if let Err(e) = self
            .ev_writer
            .send(Control::NewStream(Box::new(NewStreamParams {
                stream_id: id,
                sender,
                receiver: None,
                flow,
                window_update_sender: self.window_update_sender.clone(),
            })))
            .await
        {
            return Err(anyhow::Error::new(e));
        }
        Ok(stream)
    }

    pub async fn accept_stream(&self) -> Result<MuxStream> {
        let (sender, receiver) = oneshot::channel::<Result<MuxStream>>();
        if let Err(e) = self
            .ev_writer
            .send(Control::AcceptStream(Box::new(sender)))
            .await
        {
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

    pub fn active_stream_count(&self) -> usize {
        self.active_stream_count.load(Ordering::Relaxed)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Best-effort graceful shutdown via the control channel, then force
        // the dispatcher task to exit. `close()` alone is insufficient: if the
        // channel is full or the dispatcher is blocked on a read from a
        // half-open peer, the Close message is lost or never consumed. Aborting
        // the task guarantees the underlying socket is dropped and the FD
        // released — critical for the reconnect path, which fires close()
        // before replacing the slot.
        let _ = self.ev_writer.try_send(Control::Close);
        self.dispatch_task.abort();
    }
}

/// Remove `stream_entries` whose MuxStream receiver has been dropped.
///
/// `MuxStream::drop` sends `Control::StreamClose` via `try_send`; when the
/// control channel is saturated this fails silently and the `StreamEntry`
/// stays in the map forever. Over time, leaked entries accumulate toward
/// `MAX_MUX_STREAMS_PER_CONNECTION`, after which every new stream is rejected
/// — a slow, connection-lifetime DoS. The dispatcher calls this on its
/// periodic metrics tick to reclaim leaked slots.
///
/// Detection relies on `UnboundedSender::is_closed()`, which is `true` once
/// the receiver (held by `MuxStream::inbound_reader`) is dropped. Entries
/// still in active use keep their receiver alive and are left untouched.
///
/// Returns the number of entries removed. Updates `mux.streams` gauge and
/// `active_stream_count` so admission checks reflect reality.
fn reap_closed_stream_entries(
    stream_entries: &mut HashMap<u32, StreamEntry>,
    active_stream_count: &AtomicUsize,
) -> usize {
    let before = stream_entries.len();
    stream_entries.retain(|sid, entry| {
        if entry.sender.is_closed() {
            entry.flow.close();
            metrics::gauge!("mux.streams").decrement(1.0);
            tracing::debug!(
                "reaped stale stream entry (receiver dropped, StreamClose lost): sid={}",
                sid
            );
            false
        } else {
            true
        }
    });
    let reaped = before - stream_entries.len();
    if reaped > 0 {
        active_stream_count.fetch_sub(reaped, Ordering::Relaxed);
    }
    reaped
}

async fn apply_window_update<W: AsyncWrite + Unpin>(
    sid: u32,
    increment: u32,
    stream_entries: &mut HashMap<u32, StreamEntry>,
    w: &mut W,
    initial_stream_window: u32,
) -> anyhow::Result<()> {
    if let Some(entry) = stream_entries.get_mut(&sid) {
        entry.pending_bytes = entry.pending_bytes.saturating_sub(increment as u64);
        entry.recv_window = entry
            .recv_window
            .saturating_add(increment)
            .min(initial_stream_window);
        let ev = event::new_window_update_event(sid, increment);
        event::write_event(w, ev).await?;
    }
    Ok(())
}

/// Remove stream entries whose dispatcher-side backlog exceeded
/// `PENDING_BYTES_CAP_WINDOW_MULTIPLE × initial_stream_window`, returning
/// their stream ids. The caller is responsible for the follow-up teardown
/// (flow close, peer FIN, metrics). Split out of the dispatcher loop so the
/// threshold logic is directly unit-testable.
fn drain_backlog_capped_streams(
    stream_entries: &mut HashMap<u32, StreamEntry>,
    active_stream_count: &AtomicUsize,
    initial_stream_window: u32,
) -> Vec<u32> {
    let pending_cap = initial_stream_window as u64 * PENDING_BYTES_CAP_WINDOW_MULTIPLE;
    let stalled: Vec<u32> = stream_entries
        .iter()
        .filter(|(_, entry)| entry.pending_bytes > pending_cap)
        .map(|(sid, _)| *sid)
        .collect();
    for sid in &stalled {
        if let Some(entry) = stream_entries.remove(sid) {
            tracing::warn!(
                "stream backlog {} bytes exceeds cap {}; closing unresponsive stream (sid={})",
                entry.pending_bytes,
                pending_cap,
                sid
            );
            entry.flow.close();
            let _ = entry.sender.send(None);
            metrics::gauge!("mux.streams").decrement(1.0);
            metrics::counter!("mux.stream.backlog_cap_hit").increment(1);
        }
    }
    if !stalled.is_empty() {
        active_stream_count.fetch_sub(stalled.len(), Ordering::Relaxed);
    }
    stalled
}

#[allow(clippy::too_many_arguments)]
async fn handle_mux_connection<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    conn_id: u32,
    r: R,
    mut w: W,
    mut ev_reader: mpsc::Receiver<Control>,
    ev_writer_orig: mpsc::Sender<Control>,
    initial_stream_window: u32,
    pong_sender: mpsc::Sender<u32>,
    window_update_sender: mpsc::UnboundedSender<(u32, u32)>,
    mut window_update_receiver: mpsc::UnboundedReceiver<(u32, u32)>,
    active_stream_count: Arc<AtomicUsize>,
) {
    let ev_writer = ev_writer_orig.clone();
    let read_connection_fut = async move {
        let mut buf_reader = tokio::io::BufReader::new(r);
        while let Ok(ev) = event::read_event(&mut buf_reader).await {
            let ctrl = match ev.header.flags() {
                event::FLAG_SYN => {
                    let (sender, receiver) = mpsc::unbounded_channel::<Option<Bytes>>();
                    let flow = Arc::new(StreamFlow::new(initial_stream_window));
                    Control::NewStream(Box::new(NewStreamParams {
                        stream_id: ev.header.stream_id,
                        sender,
                        receiver: Some(receiver),
                        flow,
                        window_update_sender: window_update_sender.clone(),
                    }))
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
        // Sample flow-control metrics every 1s instead of on every control
        // message. With MAX_MUX_STREAMS_PER_CONNECTION = 1024, the previous
        // per-message iteration cost 1024 atomic loads + 3 registry lookups
        // per frame, which dominates at high frame rates.
        let mut metrics_interval = tokio::time::interval(Duration::from_millis(1000));
        metrics_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Discard the first immediate tick.
        metrics_interval.tick().await;

        loop {
            // Wait for either a window update or a control message (or the
            // periodic metrics tick). Using select! is critical: blocking on
            // ev_reader.recv() alone would never wake up when a window update
            // arrives, leading to a flow-control deadlock.
            let ctrl = tokio::select! {
                // Window update arrived — process it and drain any additional
                // pending updates before looping back.
                Some((sid, increment)) = window_update_receiver.recv() => {
                    if let Err(e) = apply_window_update(
                        sid,
                        increment,
                        &mut stream_entries,
                        &mut w,
                        initial_stream_window,
                    ).await {
                        tracing::error!("write window update failed:{}", e);
                        break;
                    }
                    while let Ok((sid, inc)) = window_update_receiver.try_recv() {
                        if let Err(e) = apply_window_update(
                            sid,
                            inc,
                            &mut stream_entries,
                            &mut w,
                            initial_stream_window,
                        ).await {
                            tracing::error!("write window update failed:{}", e);
                            break;
                        }
                    }
                    continue;
                }

                result = ev_reader.recv() => result,

                // Periodically aggregate flow-control gauges across all streams.
                // Sampled at most once per second rather than once per frame.
                _ = metrics_interval.tick() => {
                    // Reap entries whose MuxStream dropped while the control
                    // channel was saturated (StreamClose try_send failed).
                    // See `reap_closed_stream_entries` for the leak this fixes.
                    reap_closed_stream_entries(&mut stream_entries, &active_stream_count);

                    // Close streams whose dispatcher-side backlog has blown
                    // past PENDING_BYTES_CAP_WINDOW_MULTIPLE × window. The
                    // inbound channel is unbounded, so a stuck consumer would
                    // otherwise grow its queue without limit. 1s cadence is
                    // fast enough to catch a stalled stream before it
                    // accumulates many multiples of the cap.
                    let stalled = drain_backlog_capped_streams(
                        &mut stream_entries,
                        &active_stream_count,
                        initial_stream_window,
                    );
                    for sid in stalled {
                        tracing::warn!(
                            "[{}/{}] stream closed by backlog cap",
                            conn_id,
                            sid
                        );
                        let ev = event::new_fin_event(sid);
                        if let Err(e) = event::write_event(&mut w, ev).await {
                            tracing::error!("write fin for backlog-capped stream failed:{}", e);
                            break;
                        }
                    }

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
                    metrics::gauge!("mux.stream.total_pending_bytes")
                        .set(total_pending_bytes as f64);
                    continue;
                }
            };

            match ctrl {
                None => break,
                Some(ctrl) => match ctrl {
                    Control::AcceptStream(callback) => {
                        if accept_callback.is_some() {
                            let _ = callback.send(Err(anyhow!("duplicate accept")));
                            continue;
                        }
                        accept_callback = Some(*callback);
                    }
                    Control::NewStream(params) => {
                        let is_incoming = params.receiver.is_some();
                        let current_active_streams = stream_entries.len();
                        if is_incoming
                            && let Some(reason) = incoming_stream_rejection_reason(
                                incoming_streams.len(),
                                current_active_streams,
                            )
                        {
                            metrics::counter!("mux.incoming_streams_rejected", "reason" => reason)
                                .increment(1);
                            tracing::warn!(
                                "[{}/{}] reject incoming stream: {} limit reached",
                                conn_id,
                                params.stream_id,
                                reason
                            );
                            let ev = event::new_fin_event(params.stream_id);
                            let _ = event::write_event(&mut w, ev).await;
                            continue;
                        }
                        match stream_entries.entry(params.stream_id) {
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
                                active_stream_count.fetch_add(1, Ordering::Relaxed);
                                if let Some(rx) = params.receiver {
                                    let stream = MuxStream::new(
                                        conn_id,
                                        params.stream_id,
                                        ev_writer.clone(),
                                        rx,
                                        params.flow,
                                        initial_stream_window,
                                        params.window_update_sender,
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
                        }
                    }
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
                                    active_stream_count.fetch_sub(1, Ordering::Relaxed);
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
                                        active_stream_count.fetch_sub(1, Ordering::Relaxed);
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
                            active_stream_count.fetch_sub(1, Ordering::Relaxed);
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
                },
            } // close match ctrl outer (match ctrl -> None|Some)

            if accept_callback.is_some() && !incoming_streams.is_empty() {
                let stream = incoming_streams.pop_front().unwrap();
                let _ = accept_callback.unwrap().send(Ok(stream));
                accept_callback = None;
            }
        }

        metrics::gauge!("mux.streams").decrement(stream_entries.len() as f64);
        for (_, entry) in stream_entries.drain() {
            entry.flow.close();
            let _ = entry.sender.send(None);
        }
        active_stream_count.store(0, Ordering::Relaxed);
        if let Some(cb) = accept_callback {
            let _ = cb.send(Err(anyhow!("connection closed")));
        }
    };
    // Either future exiting must cancel the other. Previously this was
    // `tokio::join!`, which waits for BOTH futures — when `Connection::close()`
    // sends `Control::Close`, `read_ctrl_fut` exits but `read_connection_fut`
    // stays blocked on `event::read_event` waiting for peer data. If the peer
    // never closes TCP (half-open link, the exact case ping detection is meant
    // to catch), the dispatcher never exits and the socket FD leaks. Each
    // reconnect leaked one FD until `os error 24` (EMFILE), which then surfaced
    // as `failed to read certificate chain` (std::fs::read needs an FD).
    tokio::pin!(read_connection_fut, read_ctrl_fut);
    tokio::select! {
        _ = &mut read_connection_fut => {
            // Read side finished (peer closed or error). It already sent
            // Control::Close before completing; drain the ctrl side so its
            // cleanup (stream_entries drain, accept_callback notify) runs.
            let _ = (&mut read_ctrl_fut).await;
        }
        _ = &mut read_ctrl_fut => {
            // Ctrl side finished (Control::Close received, or all senders
            // dropped). Dropping read_connection_fut releases `r: R` and the
            // underlying TCP/TLS socket — no FD leak.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a StreamEntry with the given backlog, plus its receiver so the
    /// sender isn't immediately closed.
    fn entry_with_backlog(
        pending_bytes: u64,
    ) -> (StreamEntry, mpsc::UnboundedReceiver<Option<Bytes>>) {
        let (tx, rx) = mpsc::unbounded_channel::<Option<Bytes>>();
        (
            StreamEntry {
                sender: tx,
                recv_window: INITIAL_STREAM_WINDOW,
                flow: Arc::new(StreamFlow::new(INITIAL_STREAM_WINDOW)),
                pending_bytes,
            },
            rx,
        )
    }

    /// A stream whose consumer stops reading must not accumulate an
    /// unbounded dispatcher-side backlog. Once `pending_bytes` exceeds
    /// `PENDING_BYTES_CAP_WINDOW_MULTIPLE × initial_stream_window`,
    /// `drain_backlog_capped_streams` removes it and returns its sid for FIN
    /// teardown; below the cap the stream is left alone.
    #[test]
    fn backlog_cap_removes_only_stalled_streams() {
        let mut entries: HashMap<u32, StreamEntry> = HashMap::new();
        let active = Arc::new(AtomicUsize::new(0));
        let window = INITIAL_STREAM_WINDOW;
        let cap = window as u64 * PENDING_BYTES_CAP_WINDOW_MULTIPLE;

        // sid 0: healthy stream with a normal in-flight backlog (< cap).
        let (healthy, _rx0) = entry_with_backlog(window as u64);
        entries.insert(0, healthy);
        active.fetch_add(1, Ordering::Relaxed);

        // sid 1: stuck consumer — backlog just past the cap.
        let (stalled, _rx1) = entry_with_backlog(cap + 1);
        entries.insert(1, stalled);
        active.fetch_add(1, Ordering::Relaxed);

        let removed = drain_backlog_capped_streams(&mut entries, &active, window);

        assert_eq!(removed, vec![1], "only the over-cap stream is drained");
        assert!(entries.contains_key(&0), "healthy stream must remain");
        assert!(!entries.contains_key(&1), "stalled stream must be removed");
        assert_eq!(
            active.load(Ordering::Relaxed),
            1,
            "active count decremented by removed entries"
        );
    }

    /// Backlog at exactly the cap is not enough — the check is strictly
    /// greater-than, so a stream hovering at the boundary isn't killed.
    #[test]
    fn backlog_at_cap_is_not_drained() {
        let mut entries: HashMap<u32, StreamEntry> = HashMap::new();
        let active = Arc::new(AtomicUsize::new(1));
        let window = INITIAL_STREAM_WINDOW;
        let cap = window as u64 * PENDING_BYTES_CAP_WINDOW_MULTIPLE;

        let (entry, _rx) = entry_with_backlog(cap);
        entries.insert(7, entry);

        let removed = drain_backlog_capped_streams(&mut entries, &active, window);
        assert!(removed.is_empty());
        assert!(entries.contains_key(&7));
        assert_eq!(active.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn incoming_stream_limits_reject_pending_and_active_overflow() {
        assert_eq!(incoming_stream_rejection_reason(0, 0), None);
        assert_eq!(
            incoming_stream_rejection_reason(MAX_PENDING_INCOMING_STREAMS, 0),
            Some("pending")
        );
        assert_eq!(
            incoming_stream_rejection_reason(0, MAX_MUX_STREAMS_PER_CONNECTION),
            Some("active")
        );
    }

    /// `reap_closed_stream_entries` removes entries whose MuxStream receiver
    /// has been dropped (sender.is_closed()), as happens when
    /// `MuxStream::drop`'s `try_send(Control::StreamClose)` failed because the
    /// control channel was full. Without reaping, these entries accumulate in
    /// `stream_entries` forever, eventually hitting MAX_MUX_STREAMS_PER_CONNECTION
    /// and rejecting all new streams (DoS).
    #[test]
    fn reap_removes_entries_whose_receiver_dropped() {
        let mut entries: HashMap<u32, StreamEntry> = HashMap::new();
        let active_stream_count = Arc::new(AtomicUsize::new(0));

        // Entry 0: live stream (receiver still held) — must NOT be reaped.
        let (live_tx, _live_rx) = mpsc::unbounded_channel::<Option<Bytes>>();
        entries.insert(
            0,
            StreamEntry {
                sender: live_tx,
                recv_window: INITIAL_STREAM_WINDOW,
                flow: Arc::new(StreamFlow::new(INITIAL_STREAM_WINDOW)),
                pending_bytes: 0,
            },
        );
        active_stream_count.fetch_add(1, Ordering::Relaxed);

        // Entry 1: dead stream (receiver already dropped) — must be reaped.
        // This is exactly the state left behind when StreamClose's try_send
        // failed: the MuxStream is gone but stream_entries still holds the entry.
        let (dead_tx, dead_rx) = mpsc::unbounded_channel::<Option<Bytes>>();
        drop(dead_rx);
        entries.insert(
            1,
            StreamEntry {
                sender: dead_tx,
                recv_window: INITIAL_STREAM_WINDOW,
                flow: Arc::new(StreamFlow::new(INITIAL_STREAM_WINDOW)),
                pending_bytes: 0,
            },
        );
        active_stream_count.fetch_add(1, Ordering::Relaxed);

        assert_eq!(entries.len(), 2);
        assert_eq!(active_stream_count.load(Ordering::Relaxed), 2);

        let reaped = reap_closed_stream_entries(&mut entries, &active_stream_count);

        assert_eq!(reaped, 1, "exactly the dead entry should be reaped");
        assert!(entries.contains_key(&0), "live entry must remain");
        assert!(!entries.contains_key(&1), "dead entry must be removed");
        assert_eq!(
            active_stream_count.load(Ordering::Relaxed),
            1,
            "active_stream_count must be decremented by reaped count"
        );
    }

    /// A live entry whose receiver is then dropped between reap calls must be
    /// removed on the next reap — verifies reaping is not a one-shot and the
    /// dispatcher's periodic tick eventually clears leaked entries.
    #[test]
    fn reap_is_idempotent_and_progressive() {
        let mut entries: HashMap<u32, StreamEntry> = HashMap::new();
        let active_stream_count = Arc::new(AtomicUsize::new(0));

        let (tx, rx) = mpsc::unbounded_channel::<Option<Bytes>>();
        entries.insert(
            7,
            StreamEntry {
                sender: tx,
                recv_window: INITIAL_STREAM_WINDOW,
                flow: Arc::new(StreamFlow::new(INITIAL_STREAM_WINDOW)),
                pending_bytes: 0,
            },
        );
        active_stream_count.fetch_add(1, Ordering::Relaxed);

        // First reap: still live, nothing removed.
        assert_eq!(
            reap_closed_stream_entries(&mut entries, &active_stream_count),
            0
        );
        assert!(entries.contains_key(&7));

        // Now drop the receiver — entry becomes reapable.
        drop(rx);

        // Second reap: removed.
        assert_eq!(
            reap_closed_stream_entries(&mut entries, &active_stream_count),
            1
        );
        assert!(entries.is_empty());
        assert_eq!(active_stream_count.load(Ordering::Relaxed), 0);

        // Third reap on empty map: no-op, no panic.
        assert_eq!(
            reap_closed_stream_entries(&mut entries, &active_stream_count),
            0
        );
    }

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

    /// With at least one open (never-serviced) stream, `ping()` short-circuits
    /// on active-traffic liveness and returns Ok even though the peer never
    /// echoes pongs. This is what keeps both health pings and tunnel-control
    /// pings from false-timing-out behind saturated DATA frames.
    ///
    /// `open_stream` only *queues* `Control::NewStream`; the dispatcher
    /// increments `active_stream_count` when it processes that message. Poll
    /// `has_active_streams` (rather than sleeping a fixed interval) so the
    /// test doesn't depend on dispatcher scheduling latency.
    #[tokio::test]
    async fn ping_short_circuits_when_streams_active() {
        // Peer end is kept alive but never serviced — no PONG will ever come.
        let (a, _b) = tokio::io::duplex(8192);
        let (a_r, a_w) = tokio::io::split(a);

        let mut client =
            Connection::new_with_stream_window(a_r, a_w, Mode::Client, 0, INITIAL_STREAM_WINDOW);

        // Open a stream and leak it so active_stream_count stays > 0.
        let stream = client.open_stream().await.expect("open_stream");
        std::mem::forget(stream);

        let wait_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !client.has_active_streams() {
            assert!(
                tokio::time::Instant::now() < wait_deadline,
                "dispatcher never activated the stream"
            );
            tokio::task::yield_now().await;
        }

        // Returns immediately instead of waiting out PING_TIMEOUT.
        let start = tokio::time::Instant::now();
        client
            .ping()
            .await
            .expect("ping with active streams must succeed without a pong");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "ping should short-circuit, not wait for a pong"
        );
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

    /// Regression test for the FD-leak bug: after `close()` (or Drop), the
    /// dispatcher task must exit promptly even when the peer never closes the
    /// TCP link — exactly the half-open case ping detection surfaces.
    ///
    /// Before the fix, `tokio::join!` waited for BOTH `read_connection_fut`
    /// (blocked on peer read) and `read_ctrl_fut` (exited on Close), so the
    /// dispatcher hung forever and the socket FD leaked. With `select!` +
    /// `Drop`-abort, the task exits as soon as either side finishes.
    #[tokio::test]
    async fn dispatcher_exits_after_close_even_when_peer_silent() {
        use tokio::io::AsyncReadExt;
        // duplex whose peer end we keep alive but never service — simulates
        // a half-open TCP link where the peer never sends FIN.
        let (a, mut b) = tokio::io::duplex(8192);
        let (a_r, a_w) = tokio::io::split(a);

        let conn =
            Connection::new_with_stream_window(a_r, a_w, Mode::Client, 0, INITIAL_STREAM_WINDOW);
        // Signal the dispatcher to exit. With join! this would leave
        // read_connection_fut blocked; with select! + Drop-abort the task
        // must terminate and release the socket.
        conn.close();
        drop(conn);

        // The peer read returns 0 (EOF) once the dispatcher's `a` side is
        // dropped. Give the runtime a chance to schedule the abort/cleanup.
        let mut buf = [0u8; 1];
        let read_result = tokio::time::timeout(Duration::from_secs(2), b.read(&mut buf)).await;
        assert!(
            read_result.is_ok(),
            "peer read did not complete within 2s; dispatcher likely still holds the socket (FD leak)"
        );
        let n = read_result.unwrap().expect("read should not error");
        assert_eq!(n, 0, "expected EOF (n=0) after dispatcher exit, got n={n}");
        let _ = b;
    }

    /// End-to-end flow-control test:
    /// Client writes TRANSFER_SIZE bytes through a stream, server echoes
    /// them back, and client verifies the round-trip.  The test validates
    /// that window-update credits flow in both directions without
    /// deadlocking, even when the initial send window (SMALL_WINDOW =
    /// 4096) is much smaller than the transfer size.
    #[tokio::test]
    async fn window_update_duplex() {
        const TRANSFER_SIZE: usize = 64 * 1024; // 64 KiB
        const SMALL_WINDOW: u32 = 4096;
        const DEADLINE: Duration = Duration::from_secs(5);

        let (a, b) = tokio::io::duplex(8192);
        let (a_r, a_w) = tokio::io::split(a);
        let (b_r, b_w) = tokio::io::split(b);

        let mut client =
            Connection::new_with_stream_window(a_r, a_w, Mode::Client, 0, SMALL_WINDOW);
        let server = Connection::new_with_stream_window(b_r, b_w, Mode::Server, 1, SMALL_WINDOW);

        // Quick health check — verifies basic connectivity.
        client.ping().await.expect("ping before transfer");

        // Open stream (client side).
        let client_stream = tokio::time::timeout(DEADLINE, client.open_stream())
            .await
            .expect("timeout")
            .expect("open_stream");

        // Accept stream (server side).
        let server_stream = tokio::time::timeout(DEADLINE, server.accept_stream())
            .await
            .expect("timeout")
            .expect("accept_stream");

        let (mut cr, cw) = tokio::io::split(client_stream);
        let (sr, sw) = tokio::io::split(server_stream);

        // Spawn writer: client -> server.
        let payload = vec![0xABu8; TRANSFER_SIZE];
        let payload_clone = payload.clone();
        let writer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut cw = cw;
            cw.write_all(&payload_clone).await.expect("write_all");
            cw.shutdown().await.expect("shutdown");
        });

        // Spawn echo: server reads then writes back.
        let echo = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncWriteExt;
            let (mut sr, mut sw) = (sr, sw);
            let mut buf = vec![0u8; 8192];
            let mut total = 0usize;
            loop {
                let n = sr.read(&mut buf).await.expect("echo read");
                if n == 0 {
                    break;
                }
                sw.write_all(&buf[..n]).await.expect("echo write");
                total += n;
            }
            sw.shutdown().await.expect("shutdown");
            total
        });

        // Client reads echoed data back.
        use tokio::io::AsyncReadExt;
        let mut echo_buf = Vec::with_capacity(TRANSFER_SIZE);
        tokio::time::timeout(DEADLINE, cr.read_to_end(&mut echo_buf))
            .await
            .expect("timeout")
            .expect("client read echo");

        tokio::time::timeout(DEADLINE, writer)
            .await
            .expect("timeout")
            .expect("writer task");
        let nread = tokio::time::timeout(DEADLINE, echo)
            .await
            .expect("timeout")
            .expect("echo task");

        assert_eq!(echo_buf.len(), TRANSFER_SIZE, "echoed data length");
        assert_eq!(nread, TRANSFER_SIZE, "server echo count");
        assert_eq!(&echo_buf, &payload, "echoed data matches");
    }
}
