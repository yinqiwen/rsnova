# TLS Mux Flow Control — Unbounded Channels + WINDOW_UPDATE

## Context

The current TLS mux dispatcher (`read_ctrl_fut` in `src/mux/connection.rs`) processes all control messages in a single loop. When delivering data to a per-stream bounded channel via `stream_sender.send(data).await`, the entire dispatcher stalls if that channel is full — no other stream's data can be delivered, no outbound events can be written. This is head-of-line (HOL) blocking at the mux layer.

### Blocking points in current `src/mux/connection.rs`

| Line | Code | Impact |
|------|------|--------|
| 170 | `e.get().send(Some(Bytes::new())).await` | Blocks dispatcher on duplicate stream ID |
| 192 | `stream_sender.send(Some(data)).await` | **Core issue** — blocks dispatcher when per-stream channel full |
| 237 | `sender.send(Some(Bytes::new())).await` | Blocks dispatcher on EOF/shutdown delivery |
| 250 | `sender.send(None).await` | Blocks dispatcher on stream close |
| 280 | `sender.send(None).await` (connection close loop) | Blocks connection teardown |

### Root cause

Per-stream channels are bounded (`mpsc::channel(stream_channel_size)`, default 16). A slow consumer (e.g., a target server reading slowly) fills the channel. The dispatcher awaits on `send()`, blocking ALL streams multiplexed on the same connection.

## Goals

1. Eliminate HOL blocking: a slow stream must never block unrelated streams or the transport write path.
2. Add per-stream flow control (WINDOW_UPDATE) to propagate backpressure end-to-end through the tunnel to the remote peer.
3. Minimize changes: preserve the existing dispatcher loop architecture, `MuxConnection` trait API, and QUIC code path.

## Constraints

- `MuxConnection` trait (defined in `src/tunnel/client.rs`) unchanged — callers unaffected
- `MuxStream` continues to implement `AsyncRead + AsyncWrite`
- QUIC code path (`s2n_quic_client.rs`, `s2n_quic_remote.rs`) untouched
- Existing `Control` channel architecture (single dispatcher loop) preserved
- No new spawned tasks; no new files required (all changes fit in existing files)

---

## Design Overview

```
┌────────────────────────────────────────────────────────────────────────────┐
│                     Dispatcher Task (unchanged structure)                   │
│                                                                            │
│  read_connection_fut                  read_ctrl_fut                         │
│  ┌──────────────────┐                ┌─────────────────────────────┐       │
│  │ Wire → read_event │──Control ch──►│ dispatch Control messages   │       │
│  │                   │   (bounded    │                             │       │
│  │ + decode WIN_UPD  │    256)       │ StreamEntry per stream:     │       │
│  └──────────────────┘                │   .sender (Unbounded!)     │       │
│                                      │   .recv_window (local u32) │       │
│                                      │   .flow (Arc<StreamFlow>)  │       │
│                                      │                             │       │
│                                      │ On DATA: check recv_window │       │
│                                      │   → send() [sync, O(1)]    │       │
│                                      │                             │       │
│                                      │ On WIN_UPDATE (inbound):    │       │
│                                      │   → flow.credit()          │       │
│                                      │   → wakes MuxStream writer │       │
│                                      │                             │       │
│                                      │ On WIN_UPDATE (outbound):   │       │
│                                      │   → write FLAG_WIN_UPDATE   │       │
│                                      └─────────────────────────────┘       │
└────────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│ MuxStream                                                    │
│                                                              │
│  poll_read:                                                  │
│    unbounded_rx.poll_recv() → data                          │
│    consumed_since_update += n                                │
│    if threshold → ev_writer.get_ref().try_send(WindowUpdate)│
│                                                              │
│  poll_write:                                                 │
│    flow.available() → peek send_window                      │
│    if 0 → register waker, Pending                           │
│    poll_reserve → reserve control channel slot               │
│    flow.try_consume(len) → CAS on AtomicU32                 │
│    send_item(Control::StreamData)                           │
│                                                              │
└─────────────────────────────────────────────────────────────┘
```

### Key design decisions

| Aspect | Current | New |
|--------|---------|-----|
| Per-stream channel | `mpsc::channel(16)` bounded | `mpsc::unbounded_channel()` |
| Dispatcher DATA delivery | `sender.send(data).await` — **blocks** | `sender.send(data)` — sync, O(1), **never blocks** |
| Slow stream impact | Blocks entire dispatcher | Only its own MuxStream poll_read backs up |
| Memory protection | Channel backpressure (implicit) | `recv_window` — explicit, per-stream, protocol-level |
| Backpressure propagation | Local only (channel full) | End-to-end via WINDOW_UPDATE to remote peer |
| Send-side gating | None | `send_window` — MuxStream poll_write returns Pending when exhausted |
| Shared state per stream | None | `Arc<StreamFlow>` — AtomicU32 + AtomicBool + AtomicWaker |
| New locks on hot path | N/A | **Zero** — entire write path is lock-free (CAS + AtomicWaker) |

---

## Backpressure Propagation (End-to-End)

Since this is a proxy/tunnel, backpressure must propagate from the local consumer all the way to the remote source:

```
Target Server          Remote Proxy              Local Proxy           Client App
     │                     │                         │                     │
     ▼ TCP write           ▼ MuxStream write         ▼ MuxStream read      ▼ TCP read (slow)
     │                     │                         │                     │
     │     send_window=0   │                         │  not reading        │
     │     poll_write      │◄── no WINDOW_UPDATE ────│  consumed=0         │
     │     → Pending       │                         │  → no WIN_UPD sent  │
     │         │           │                         │                     │
     │    copy loop stops  │                         │                     │
     │    reading Target   │                         │                     │
     │         │           │                         │                     │
     ◄─── TCP backpressure─┘                         │                     │
     (Target forced to                               │                     │
      slow down)                                     │                     │
```

**Chain of causation:**
1. Client App reads slowly → local copy loop blocks on `dst.write()` (TCP backpressure)
2. Local copy loop stops calling `src.read()` on MuxStream
3. MuxStream not polled for read → `consumed_since_update` never reaches threshold → no WINDOW_UPDATE sent
4. Remote's `send_window` for this stream hits 0
5. Remote MuxStream `poll_write` returns `Pending`
6. Remote copy loop stops calling `src.read()` on Target connection
7. TCP backpressure to Target Server → Target slows down

Each stream is independent — a slow Client App on stream A does not affect stream B's throughput.

---

## Wire Protocol Addition

### FLAG_WIN_UPDATE

Value `8` is available (historical `FLAG_PONG` was removed):

```rust
pub const FLAG_WIN_UPDATE: u8 = 8;
```

Wire format:

```text
+--------+--------+--------+--------+--------+--------+--------+--------+
|     flag_len (4 bytes LE)         |     stream_id (4 bytes LE)        |
+--------+--------+--------+--------+--------+--------+--------+--------+
|     increment (4 bytes LE)        |
+--------+--------+--------+--------+
```

- `flag_len`: `(4 << 8) | FLAG_WIN_UPDATE` — body is 4 bytes
- `stream_id`: target stream
- `increment`: bytes the receiver has consumed and is now willing to accept again

Event constructor (added to `src/mux/event.rs`):

```rust
pub fn new_window_update_event(sid: u32, increment: u32) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(4, FLAG_WIN_UPDATE),
            stream_id: sid,
        },
        body: Bytes::copy_from_slice(&increment.to_le_bytes()),
    }
}
```

### Flow control scope

Only `FLAG_DATA` frames are subject to `recv_window` checks. Other data-carrying flags (`FLAG_AUTH`, `FLAG_AUTH_ACK`, `FLAG_OPEN`, `FLAG_REVERSE_OPEN`) are control frames that bypass flow control — they are small, infrequent, and occur during stream setup.

---

## Data Structures

### StreamFlow (new, added to `src/mux/stream.rs`)

Shared between dispatcher and MuxStream. Per-stream. Uses `AtomicWaker` from the `futures` crate (already a project dependency) to eliminate all Mutex usage.

```rust
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use futures::task::AtomicWaker;

/// Per-stream flow control state shared between dispatcher and MuxStream.
/// The dispatcher credits send_window (on inbound WINDOW_UPDATE) and sets closed (on connection close).
/// MuxStream deducts send_window (on poll_write) and registers write_waker.
pub struct StreamFlow {
    /// Bytes the peer allows us to send. Deducted by poll_write (CAS).
    /// Credited by dispatcher on receiving peer's WINDOW_UPDATE.
    send_window: AtomicU32,
    /// Set by dispatcher on connection close. Checked by poll_write to return BrokenPipe.
    closed: AtomicBool,
    /// Waker registered by poll_write when send_window == 0.
    /// Woken by dispatcher when crediting send_window or closing.
    /// AtomicWaker is lock-free; wake() on an unregistered waker is a no-op.
    write_waker: AtomicWaker,
}

impl StreamFlow {
    pub fn new(initial_window: u32) -> Self {
        Self {
            send_window: AtomicU32::new(initial_window),
            closed: AtomicBool::new(false),
            write_waker: AtomicWaker::new(),
        }
    }

    /// Returns current send_window value (peek, no deduction).
    /// Returns 0 if closed. Used by poll_write to check before reserving control channel slot.
    pub fn available(&self) -> u32 {
        if self.closed.load(Ordering::Acquire) {
            return 0;
        }
        self.send_window.load(Ordering::Acquire)
    }

    /// Called by dispatcher when peer sends WINDOW_UPDATE.
    /// Adds `increment` to send_window and wakes blocked writer.
    pub fn credit(&self, increment: u32) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        self.send_window.fetch_add(increment, Ordering::Release);
        self.write_waker.wake(); // no-op if no waker registered
    }

    /// Called by MuxStream::poll_write. Attempts to deduct up to `desired` bytes
    /// from send_window using CAS. Returns actual bytes available (0 = must Pending).
    /// Returns 0 if stream is closed.
    pub fn try_consume(&self, desired: usize) -> usize {
        if self.closed.load(Ordering::Acquire) {
            return 0;
        }
        loop {
            let current = self.send_window.load(Ordering::Acquire);
            if current == 0 {
                return 0;
            }
            let n = desired.min(current as usize);
            match self.send_window.compare_exchange(
                current,
                current - n as u32,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return n,
                Err(_) => continue, // retry — only fails if credit() raced, benign
            }
        }
    }

    /// Called by MuxStream::poll_write when available() returns 0.
    pub fn register_waker(&self, waker: &Waker) {
        self.write_waker.register(waker);
    }

    /// Returns true if the stream has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Called by dispatcher when connection is closing.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.write_waker.wake(); // wake blocked writer so it observes closed
    }
}
```

**Contention analysis:**
- `available` (poll_write fast path): **lock-free AtomicU32 load** + AtomicBool load. Zero CAS, zero locks.
- `try_consume` (poll_write, after slot reserved): **lock-free CAS** on AtomicU32. Only one MuxStream writes (single owner), so CAS fails only when `credit()` races — extremely rare.
- `credit` (dispatcher, on WINDOW_UPDATE receipt): `fetch_add` on AtomicU32 (lock-free). `AtomicWaker::wake()` is lock-free — no-op if no waker registered.
- `register_waker` (backpressure path only): `AtomicWaker::register()` — lock-free atomic swap.
- **The entire write path is completely lock-free. Zero Mutex anywhere.**

### StreamEntry (dispatcher-local, in `src/mux/connection.rs`)

```rust
/// Per-stream state owned exclusively by the dispatcher. No sharing needed.
struct StreamEntry {
    sender: mpsc::UnboundedSender<Option<Bytes>>,
    recv_window: u32,
    flow: Arc<StreamFlow>,
}
```

- `sender`: unbounded channel sender for delivering inbound data to MuxStream
- `recv_window`: bytes we still allow the peer to send. Decremented on DATA, replenished via `saturating_add` + `min(initial_stream_window)` on WindowUpdateToPeer (overflow-safe)
- `flow`: shared with MuxStream for send_window management

---

## Control Enum Changes

```rust
pub struct NewStreamParams {
    pub stream_id: u32,
    pub sender: mpsc::UnboundedSender<Option<Bytes>>,
    pub receiver: Option<StreamDataReceiver>,
    pub flow: Arc<StreamFlow>,
}

type StreamDataReceiver = mpsc::UnboundedReceiver<Option<Bytes>>;

pub enum Control {
    AcceptStream(oneshot::Sender<Result<MuxStream>>),
    NewStream(NewStreamParams),
    StreamData(u32, Bytes, bool),
    StreamShutdown(u32, bool),
    StreamClose(u32, bool),
    WindowUpdateFromPeer(u32, u32),  // peer credits our send_window; dispatcher only calls flow.credit()
    WindowUpdateToPeer(u32, u32),    // local consumed data; dispatcher updates recv_window + writes wire
    Ping,
    Close,
}
```

Changes from current:
- `NewStream` uses named struct `NewStreamParams` instead of tuple — self-documenting, extensible
- `NewStreamParams` gains `flow: Arc<StreamFlow>` field
- `mpsc::Sender` → `mpsc::UnboundedSender`; `mpsc::Receiver` → `mpsc::UnboundedReceiver`
- New variants: `WindowUpdateFromPeer` and `WindowUpdateToPeer` (instead of a single `WindowUpdate` with bool — the two directions have completely disjoint code paths, separate variants make the match branches self-explanatory)

Note: `MuxStream` does NOT gain a separate `ctrl_sender` field. It continues to use `self.ev_writer.get_ref()` (existing pattern from `Drop` impl) for non-blocking `try_send` of `WindowUpdateToPeer` and `StreamClose`.

---

## Dispatcher Changes (`src/mux/connection.rs`)

### Constants

```rust
pub const INITIAL_STREAM_WINDOW: u32 = 256 * 1024; // 256KB per stream
pub const WINDOW_UPDATE_THRESHOLD: u32 = INITIAL_STREAM_WINDOW / 2; // 128KB
```

### `read_connection_fut` changes

```rust
let read_connection_fut = async move {
    let mut buf_reader = tokio::io::BufReader::new(r);
    while let Ok(ev) = event::read_event(&mut buf_reader).await {
        let ctrl = match ev.header.flags() {
            event::FLAG_SYN => {
                // Incoming stream: create unbounded channel + flow
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
            event::FLAG_PING => continue, // no-op, same as before
            event::FLAG_WIN_UPDATE => {
                // NEW: peer is telling us we can send more
                if ev.body.len() == 4 {
                    let increment = u32::from_le_bytes(ev.body[..4].try_into().unwrap());
                    Control::WindowUpdateFromPeer(ev.header.stream_id, increment)
                } else {
                    continue; // malformed, ignore
                }
            }
            _ => {
                tracing::error!("Unexpected event:{}/{}", ev.header.flags(), ev.header.stream_id);
                continue;
            }
        };
        if ev_writer.send(ctrl).await.is_err() {
            break;
        }
    }
    let _ = ev_writer.send(Control::Close).await;
};
```

### `read_ctrl_fut` changes

```rust
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
            Control::NewStream(params) => {
                match stream_entries.entry(params.stream_id) {
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
                            // Incoming stream: create MuxStream and queue for accept
                            let stream = MuxStream::new(
                                params.stream_id, ev_writer.clone(), rx, params.flow, initial_stream_window,
                            );
                            incoming_streams.push_back(stream);
                        } else {
                            // Outgoing stream: write SYN to wire
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
                    // --- Inbound DATA: check recv_window, deliver to stream ---
                    if let Some(entry) = stream_entries.get_mut(&sid) {
                        let data_len = data.len() as u32;
                        if data_len > entry.recv_window {
                            // Flow control violation — close stream
                            tracing::error!(
                                "[{}/{}] flow control violation: {} > recv_window {}",
                                conn_id, sid, data_len, entry.recv_window
                            );
                            entry.flow.close(); // ensure poll_write immediately sees closed
                            let _ = entry.sender.send(None); // signal close
                            stream_entries.remove(&sid);
                            metrics::decrement_gauge!("mux.streams", 1.0);
                            let ev = event::new_fin_event(sid);
                            let _ = event::write_event(&mut w, ev).await;
                        } else {
                            entry.recv_window -= data_len;
                            // Unbounded send — synchronous, O(1), NEVER blocks
                            if entry.sender.send(Some(data)).is_err() {
                                tracing::error!("[{}/{}] stream receiver dropped", conn_id, sid);
                                entry.flow.close();
                                stream_entries.remove(&sid);
                                metrics::decrement_gauge!("mux.streams", 1.0);
                            }
                        }
                    }
                } else {
                    // --- Outbound DATA: write to wire (unchanged) ---
                    let ev = event::new_data_event(sid, data);
                    if let Err(e) = event::write_event(&mut w, ev).await {
                        tracing::error!("write stream data failed:{}", e);
                        break;
                    }
                }
            }
            Control::WindowUpdateFromPeer(sid, increment) => {
                // Peer sent us WINDOW_UPDATE → credit our send_window for this stream
                if let Some(entry) = stream_entries.get(&sid) {
                    entry.flow.credit(increment);
                }
            }
            Control::WindowUpdateToPeer(sid, increment) => {
                // MuxStream consumed data → tell peer they can send more
                if let Some(entry) = stream_entries.get_mut(&sid) {
                    entry.recv_window = entry.recv_window
                        .saturating_add(increment)
                        .min(initial_stream_window);
                    let ev = event::new_window_update_event(sid, increment);
                    if let Err(e) = event::write_event(&mut w, ev).await {
                        tracing::error!("write window update failed:{}", e);
                        break;
                    }
                }
                // Stream already closed — drop silently, no wire write needed
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
                        // Signal EOF to stream (empty Bytes sentinel)
                        let _ = entry.sender.send(Some(Bytes::new()));
                    }
                }
            }
            Control::StreamClose(sid, remote) => {
                if let Some(entry) = stream_entries.remove(&sid) {
                    metrics::decrement_gauge!("mux.streams", 1.0);
                    entry.flow.close(); // ensure poll_write immediately sees closed
                    if !remote {
                        let ev = event::new_fin_event(sid);
                        let _ = event::write_event(&mut w, ev).await;
                    } else {
                        let _ = entry.sender.send(None); // signal connection reset
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

        // Deliver accepted streams (unchanged logic)
        if accept_callback.is_some() && !incoming_streams.is_empty() {
            let stream = incoming_streams.pop_front().unwrap();
            let _ = accept_callback.unwrap().send(Ok(stream));
            accept_callback = None;
        }
    }

    // Close all streams (now sync, no await needed)
    metrics::decrement_gauge!("mux.streams", stream_entries.len() as f64);
    for (_, entry) in stream_entries.drain() {
        entry.flow.close();
        let _ = entry.sender.send(None);
    }
    if let Some(cb) = accept_callback {
        let _ = cb.send(Err(anyhow!("connection closed")));
    }
};
```

**Key changes highlighted:**
1. `stream_senders: HashMap<u32, Sender>` → `stream_entries: HashMap<u32, StreamEntry>`
2. `sender.send(data).await` → `sender.send(data)` (sync, unbounded — **the HOL fix**)
3. New `Control::WindowUpdateFromPeer` and `Control::WindowUpdateToPeer` handling
4. `recv_window` check before DATA delivery
5. `flow.close()` called on StreamClose — ensures poll_write immediately observes closed
6. Connection close loop is now sync (no `.await` needed for unbounded sends)

### `Connection::open_stream` changes

```rust
pub async fn open_stream(&self) -> Result<MuxStream> {
    let (sender, receiver) = mpsc::unbounded_channel::<Option<Bytes>>();
    let flow = Arc::new(StreamFlow::new(self.initial_stream_window));
    let id = self.stream_id_seed.fetch_add(2, Ordering::SeqCst);
    let stream = MuxStream::new(
        id, self.ev_writer.clone(), receiver, flow.clone(), self.initial_stream_window,
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
```

### `Connection` struct changes

```rust
pub struct Connection {
    ev_writer: mpsc::Sender<Control>,
    stream_id_seed: AtomicU32,
    initial_stream_window: u32,
}
```

Note: `stream_channel_size: usize` field is replaced by `initial_stream_window: u32`. The constructor is renamed from `new_with_stream_channel_size` to `new_with_stream_window` with the parameter changing from item count to byte count. See "Caller Migration" section below for all call-site changes.

### `handle_mux_connection` signature change

```rust
// Before:
async fn handle_mux_connection(
    r: R, w: W, mode: Mode, stream_channel_size: usize, ...
) -> ...

// After:
async fn handle_mux_connection(
    r: R, w: W, mode: Mode, stream_window: u32, ...
) -> ...
```

The `stream_window` parameter is captured by `read_connection_fut` as `initial_stream_window` (used to create `StreamFlow::new(initial_stream_window)` for incoming streams) and by `read_ctrl_fut` as the initial `recv_window` value in `StreamEntry`.

---

## MuxStream Changes (`src/mux/stream.rs`)

### Struct

```rust
pub struct MuxStream {
    id: u32,
    ev_writer: PollSender<Control>,                 // for poll_write (outbound data, shutdown)
    inbound_reader: mpsc::UnboundedReceiver<Option<Bytes>>,  // CHANGED: unbounded
    recv_buf: Bytes,
    initial_close: bool,
    close_by_remote: bool,
    read_eof: bool,
    // --- Flow control (new) ---
    flow: Arc<StreamFlow>,
    consumed_since_update: u32,
    window_update_threshold: u32,  // computed from initial_stream_window / 2 at construction
}
```

### Constructor

```rust
impl MuxStream {
    pub fn new(
        id: u32,
        ev_writer: mpsc::Sender<Control>,
        inbound_reader: mpsc::UnboundedReceiver<Option<Bytes>>,
        flow: Arc<StreamFlow>,
        initial_stream_window: u32,
    ) -> Self {
        Self {
            id,
            ev_writer: PollSender::new(ev_writer),
            inbound_reader,
            recv_buf: Bytes::new(),
            initial_close: false,
            close_by_remote: false,
            read_eof: false,
            flow,
            consumed_since_update: 0,
            window_update_threshold: initial_stream_window / 2,
        }
    }
}
```

Note: No separate `ctrl_sender` field. For non-blocking sends (WindowUpdate, Drop close), we use `self.ev_writer.get_ref()` — the same pattern already used in the current `Drop` implementation.

### AsyncRead (with WINDOW_UPDATE emission)

```rust
impl AsyncRead for MuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // 1. Drain local recv_buf first (existing logic, unchanged)
        if !self.recv_buf.is_empty() {
            let copy_n = self.recv_buf.len().min(buf.remaining());
            buf.put_slice(&self.recv_buf[..copy_n]);
            self.recv_buf = if copy_n == self.recv_buf.len() {
                Bytes::new()
            } else {
                self.recv_buf.slice(copy_n..)
            };
            self.maybe_send_window_update(copy_n as u32);
            return Poll::Ready(Ok(()));
        }

        if self.read_eof {
            self.close_reader();
            return Poll::Ready(Ok(()));
        }

        // 2. Poll unbounded channel (changed from bounded)
        match self.inbound_reader.poll_recv(cx) {
            Poll::Ready(Some(data)) => match data {
                Some(b) => {
                    let mut copy_n = b.len();
                    if copy_n == 0 {
                        self.read_eof = true;
                        self.close_reader();
                        return Poll::Ready(Ok(()));
                    }
                    if copy_n > buf.remaining() {
                        copy_n = buf.remaining();
                    }
                    buf.put_slice(&b[..copy_n]);
                    if copy_n < b.len() {
                        self.recv_buf = b.slice(copy_n..);
                    }
                    // NEW: track consumption for WINDOW_UPDATE
                    self.maybe_send_window_update(copy_n as u32);
                    Poll::Ready(Ok(()))
                }
                None => {
                    self.close_by_remote = true;
                    self.close_reader();
                    Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "close by remote",
                    )))
                }
            },
            Poll::Ready(None) => {
                self.close_reader();
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "close by remote",
                )))
            }
            Poll::Pending => {
                if self.read_eof {
                    self.close_reader();
                    return Poll::Ready(Ok(()));
                }
                if self.close_by_remote {
                    self.close_reader();
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "close by remote",
                    )));
                }
                Poll::Pending
            }
        }
    }
}
```

### WINDOW_UPDATE emission helper

```rust
impl MuxStream {
    /// Track bytes consumed by poll_read. When threshold reached,
    /// send WINDOW_UPDATE to peer via control channel (non-blocking try_send).
    fn maybe_send_window_update(&mut self, bytes_read: u32) {
        self.consumed_since_update += bytes_read;
        if self.consumed_since_update >= self.window_update_threshold {
            if let Some(sender) = self.ev_writer.get_ref() {
                // try_send is non-blocking. If control channel is full (capacity 256),
                // the update is deferred to the next read — peer's send_window will
                // recover slightly slower but won't deadlock.
                match sender.try_send(
                    Control::WindowUpdateToPeer(self.id, self.consumed_since_update)
                ) {
                    Ok(()) => {
                        self.consumed_since_update = 0; // only reset on success
                    }
                    Err(_) => {
                        // Keep consumed_since_update intact — retry on next poll_read.
                        // This prevents window leak: the increment is not lost.
                    }
                }
            }
        }
    }
}
```

**Why `try_send` is safe here:**
- Control channel capacity is 256. Under normal load, it's rarely full.
- If full, `consumed_since_update` is NOT reset — the increment is preserved for next attempt.
- On the NEXT poll_read, `consumed_since_update` grows further, and we retry. Eventually the control channel drains and the update goes through.
- Worst case: peer pauses sending briefly. No deadlock, no data loss, no window leak.

### AsyncWrite (with send_window gating)

```rust
impl AsyncWrite for MuxStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        if self.close_by_remote || self.flow.is_closed() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stream closed",
            )));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // 1. Check send_window (peek only — no deduction yet)
        if self.flow.available() == 0 {
            self.flow.register_waker(cx.waker());
            // Double-check after register to catch concurrent events:
            // - credit() → available() > 0 → fall through (normal path)
            // - close() → is_closed() true → BrokenPipe (prevents permanent
            //   Pending: close()'s wake() was a no-op because waker wasn't
            //   registered yet at that point)
            if self.flow.is_closed() {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "stream closed",
                )));
            }
            if self.flow.available() == 0 {
                return Poll::Pending;
            }
        }

        // 2. Reserve control channel slot (window not yet consumed).
        //    If Pending: no window consumed, no data to retry — clean return.
        //    Also register flow waker so close() can wake us immediately
        //    (step 1 may have been skipped when window > 0, leaving flow waker unregistered).
        match self.ev_writer.poll_reserve(cx) {
            Poll::Pending => {
                self.flow.register_waker(cx.waker());
                if self.flow.is_closed() {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "stream closed",
                    )));
                }
                return Poll::Pending;
            }
            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(utils::make_io_error(&e.to_string())));
            }
            Poll::Ready(Ok(_)) => {}
        }

        // 3. Consume send_window (guaranteed: only we deduct, only credit() adds — window
        //    can only have grown since step 1, so try_consume will succeed for ≥ peek value)
        let allowed = self.flow.try_consume(buf.len());
        if allowed == 0 {
            // Closed between peek and consume — reserved slot is discarded (harmless)
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stream closed",
            )));
        }

        // 4. Send data (slot already reserved)
        let data = Bytes::copy_from_slice(&buf[..allowed]);
        match self.ev_writer.send_item(Control::StreamData(self.id, data, false)) {
            Ok(()) => Poll::Ready(Ok(allowed)),
            Err(ex) => Poll::Ready(Err(utils::make_io_error(&ex.to_string()))),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        // ... existing implementation unchanged ...
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        // ... existing implementation unchanged ...
    }
}
```

**Why this ordering eliminates `pending_write`:**

The key insight is that `send_window` can only *grow* between `available()` and `try_consume()` — only this MuxStream deducts from it (single owner), and only `credit()` adds to it. So if `available()` returns > 0, `try_consume()` is guaranteed to succeed for at least that many bytes. No window can be "consumed but not sent."

| Step | Fails? | Window consumed? | Retry needed? |
|------|--------|------------------|---------------|
| 1. available() == 0 | yes | No | Just Pending, credit() will wake |
| 2. poll_reserve Pending | yes | No | Just Pending, PollSender will wake |
| 3. try_consume == 0 | (closed) | No | BrokenPipe |
| 4. send_item | (channel closed) | Yes but harmless | BrokenPipe |

**No `pending_write`, no `flush_or_pend`, no double-deduction, no data loss.**

### Drop

```rust
impl Drop for MuxStream {
    fn drop(&mut self) {
        if !self.close_by_remote {
            if let Some(sender) = self.ev_writer.get_ref() {
                let stream_close = Control::StreamClose(self.id, false);
                if let Err(e) = sender.try_send(stream_close) {
                    tracing::debug!("stream {} drop send close failed: {}", self.id, e);
                }
            }
        }
    }
}
```

Unchanged from current pattern — continues using `ev_writer.get_ref()` for non-blocking send in Drop.

---

## Flow Control Constants

```rust
/// Initial per-stream receive/send window. Both sides MUST use the same value.
/// Configurable via --mux-stream-window CLI arg.
///
/// 256KB: matches MAX_EVENT_BODY_LEN. One full DATA frame exactly fills the window.
/// For high-latency links, increase to 512KB-1MB to keep the pipe full.
pub const INITIAL_STREAM_WINDOW: u32 = 256 * 1024;

/// Consumption threshold before sending WINDOW_UPDATE.
/// At 50% of window: batches updates (fewer control frames) while ensuring
/// the peer's send_window never stays at 0 for more than ~1 RTT after consumer resumes.
///
/// Note: This constant is for documentation only. The actual threshold is computed
/// per-MuxStream as `initial_stream_window / 2` at construction time, so it
/// correctly adapts when --mux-stream-window is configured at runtime.
pub const WINDOW_UPDATE_THRESHOLD: u32 = INITIAL_STREAM_WINDOW / 2;
```

### Throughput vs RTT tradeoff

```
Achievable throughput ≈ INITIAL_STREAM_WINDOW / RTT

With 256KB window and 50ms RTT:  256KB / 50ms = 5 MB/s per stream
With 512KB window and 50ms RTT:  512KB / 50ms = 10 MB/s per stream
With 1MB window and 100ms RTT:   1MB / 100ms  = 10 MB/s per stream
```

For LAN/low-latency: 256KB default is sufficient.
For WAN/high-latency: users should increase `--mux-stream-window`.

---

## Files Changed

| File | Change type | Description |
|------|-------------|-------------|
| `src/mux/event.rs` | Add ~10 lines | `FLAG_WIN_UPDATE = 8` constant, `new_window_update_event()` constructor |
| `src/mux/stream.rs` | Modify ~45 lines | Add `StreamFlow` struct (with `AtomicWaker`), add `flow`/`consumed_since_update` fields, change channel to unbounded, add `maybe_send_window_update()`, add send_window check in `poll_write` |
| `src/mux/connection.rs` | Modify ~50 lines | Add `StreamEntry` struct, change channels to unbounded, add `recv_window` check in DATA handling, add `WindowUpdateFromPeer`/`WindowUpdateToPeer` handling, call `flow.close()` on StreamClose, rename constructor + field |
| `src/mux/mod.rs` | Modify 2 lines | Export `INITIAL_STREAM_WINDOW`, remove `DEFAULT_STREAM_CHANNEL_SIZE` |
| `src/main.rs` | Modify ~5 lines | Rename `--mux-stream-channel-size` → `--mux-stream-window`, change default and type |
| `src/tunnel/tls_client.rs` | Modify ~8 lines | Rename `stream_channel_size` → `stream_window`, `usize` → `u32` |
| `src/tunnel/tls_remote.rs` | Modify ~4 lines | Same rename |
| `src/tunnel/mod.rs` | Modify ~2 lines | Same rename |

**Unchanged:**
- `src/tunnel/client.rs` — `MuxConnection` trait definition unchanged
- `src/tunnel/tunnel_client.rs` — same
- `src/tunnel/tunnel_remote.rs` — same
- `src/tunnel/tunnel_registry.rs` — same
- `src/tunnel/s2n_quic_client.rs` — QUIC path untouched
- `src/tunnel/s2n_quic_remote.rs` — QUIC path untouched

---

## Migration Path

### Single-phase implementation (no intermediate states)

All changes are landed together to avoid shipping an intermediate state where unbounded channels exist without flow control (which would be an OOM risk).

#### Step 1: Core mux changes (`src/mux/`)

- Add `StreamFlow` struct to `stream.rs` (with `AtomicWaker`)
- Add `FLAG_WIN_UPDATE` constant and `new_window_update_event()` to `event.rs`
- Add `NewStreamParams` struct and `Control::WindowUpdateFromPeer`/`WindowUpdateToPeer` variants to `stream.rs`
- Change `mpsc::channel(size)` → `mpsc::unbounded_channel()` for per-stream channels
- Change `sender.send(data).await` → `sender.send(data)` in dispatcher (sync, unbounded — the HOL fix)
- Add `StreamEntry` struct with `recv_window` + `flow` fields
- Add `recv_window` check in dispatcher DATA handling (flow control enforcement)
- Add `maybe_send_window_update()` in MuxStream `poll_read`
- Add send_window check in MuxStream `poll_write`
- Handle `Control::WindowUpdateFromPeer` and `Control::WindowUpdateToPeer` in dispatcher
- Call `flow.close()` in StreamClose handler (ensures poll_write immediately sees closed)
- Rename `new_with_stream_channel_size` → `new_with_stream_window`, parameter `usize` → `u32`

#### Step 2: Caller migration

- `src/main.rs`: Rename CLI arg `--mux-stream-channel-size` → `--mux-stream-window`, default `16` → `262144`
- `src/tunnel/tls_client.rs`: Rename field `stream_channel_size: usize` → `stream_window: u32`, update all references
- `src/tunnel/tls_remote.rs`: Same rename
- `src/tunnel/mod.rs`: Same rename
- `src/mux/mod.rs`: Export `INITIAL_STREAM_WINDOW` constant, remove `DEFAULT_STREAM_CHANNEL_SIZE`

#### Step 3: Tests

- Unit tests for `StreamFlow::try_consume`, `credit`, waker behavior, `close`
- Unit test for `WINDOW_UPDATE` event encode/decode round-trip
- Integration test: fill peer's window → verify writer Pending → send WINDOW_UPDATE → verify resume

---

## Backward Compatibility

Both peers must support WINDOW_UPDATE for flow control to work.

**Phase 1 (this implementation):** All deployments update together. No version negotiation needed. This is appropriate for single-operator environments where client and server are deployed in lockstep.

**Phase 2 (future, if needed):** For gradual rollout scenarios, old peers that don't understand `FLAG_WIN_UPDATE` will hit the `_ =>` branch in their dispatcher and log an error but continue working. However, they will never send WINDOW_UPDATE, so the new peer's `send_window` will eventually hit 0 and the stream will stall.

Mitigation (deferred to Phase 2): If no WINDOW_UPDATE is received within a timeout (e.g., 30s after window exhaustion), assume the peer is old and set `send_window = u32::MAX` (disable flow control for that stream). This makes the new code fully backward-compatible with old peers at the cost of no flow control for those connections.

---

## Testing

### Unit tests

- `StreamFlow`: `available`, `try_consume` correctness, `credit` wakes waker, CAS retry under contention, `close` sets closed flag and wakes waker
- `WINDOW_UPDATE` event: encode/decode round-trip, correct flag_len encoding

### Integration tests

- **HOL blocking eliminated**: Open 2 streams. Stream A's consumer is slow (never reads). Stream B reads normally. Verify B is unaffected.
- **Backpressure propagation**: Fill recv_window on one stream → verify dispatcher rejects further DATA (flow violation). Send WINDOW_UPDATE → verify send_window credited and writer resumes.
- **Window update emission**: Read data from MuxStream → verify WINDOW_UPDATE sent after threshold.
- **Connection close**: Drop connection → verify all streams observe close, no hanging.

### CLI configuration

```text
--mux-stream-window <BYTES>     Initial per-stream flow control window in bytes (default: 262144)
```

Replaces the previous `--mux-stream-channel-size` argument.

---

## Summary

This design solves HOL blocking and adds end-to-end flow control with minimal architectural change:

- **No new tasks** (reader_task/writer_task) — keeps the single dispatcher loop
- **No new files required** — all code fits in existing `connection.rs`, `stream.rs`, `event.rs`
- **Completely lock-free write path** — AtomicU32 CAS + AtomicWaker (no Mutex anywhere)
- **No new struct fields on MuxStream** beyond flow-control state — reuses existing `ev_writer.get_ref()` pattern
- **Single-phase deployment** — no intermediate states with memory safety risks
- **~110 lines of net change** across mux files + ~20 lines of caller renames
- **Full backpressure propagation** through the tunnel to remote peer via WINDOW_UPDATE
