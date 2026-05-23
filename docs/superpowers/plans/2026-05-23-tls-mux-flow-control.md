# TLS Mux Flow Control Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate HOL blocking in the mux dispatcher by switching per-stream channels to unbounded + adding per-stream WINDOW_UPDATE flow control.

**Architecture:** Replace bounded `mpsc::channel(16)` with `mpsc::unbounded_channel()` for per-stream data delivery (makes dispatcher send sync/O(1)). Add `StreamFlow` (lock-free AtomicU32 + AtomicWaker) for send_window gating in `poll_write`. Add `recv_window` tracking in dispatcher + `FLAG_WIN_UPDATE` wire protocol for end-to-end backpressure.

**Tech Stack:** Rust, tokio, futures (AtomicWaker), bytes

**Spec:** `docs/superpowers/specs/2026-05-23-tls-mux-architecture-refactor.md`

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/mux/event.rs` | Wire protocol: add `FLAG_WIN_UPDATE` constant + `new_window_update_event()` |
| `src/mux/stream.rs` | `StreamFlow` struct, `Control` enum changes, `MuxStream` flow control logic |
| `src/mux/connection.rs` | `StreamEntry`, dispatcher changes, `Connection` struct rename |
| `src/mux/mod.rs` | Re-exports |
| `src/main.rs` | CLI arg rename |
| `src/tunnel/tls_client.rs` | Field rename |
| `src/tunnel/tls_remote.rs` | Field rename |
| `src/tunnel/mod.rs` | Parameter rename |

---

### Task 1: Add FLAG_WIN_UPDATE and event constructor

**Files:**
- Modify: `src/mux/event.rs`

- [ ] **Step 1: Write the failing test for WINDOW_UPDATE event encode/decode**

Add to the existing `#[cfg(test)] mod tests` block in `src/mux/event.rs`:

```rust
#[tokio::test]
async fn write_read_window_update_event() {
    let ev = new_window_update_event(42, 131072);
    assert_eq!(ev.header.flags(), FLAG_WIN_UPDATE);
    assert_eq!(ev.header.len(), 4);
    assert_eq!(ev.header.stream_id, 42);
    let increment = u32::from_le_bytes(ev.body[..4].try_into().unwrap());
    assert_eq!(increment, 131072);

    // Round-trip through write/read
    let mut writer = PartialVecWriter::new(1024);
    write_event(&mut writer, ev).await.unwrap();

    let mut reader = &writer.written[..];
    let decoded = read_event(&mut reader).await.unwrap();
    assert_eq!(decoded.header.flags(), FLAG_WIN_UPDATE);
    assert_eq!(decoded.header.stream_id, 42);
    let decoded_increment = u32::from_le_bytes(decoded.body[..4].try_into().unwrap());
    assert_eq!(decoded_increment, 131072);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib mux::event::tests::write_read_window_update_event`

Expected: FAIL — `FLAG_WIN_UPDATE` and `new_window_update_event` not found.

- [ ] **Step 3: Add FLAG_WIN_UPDATE constant and new_window_update_event()**

In `src/mux/event.rs`, after line 16 (`pub const FLAG_REVERSE_OPEN: u8 = 10;`), add:

```rust
pub const FLAG_WIN_UPDATE: u8 = 8;
```

After `new_ping_event()` (line 187), add:

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

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib mux::event::tests::write_read_window_update_event`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/mux/event.rs
git commit -m "feat(mux): add FLAG_WIN_UPDATE constant and new_window_update_event()"
```

---

### Task 2: Add StreamFlow struct

**Files:**
- Modify: `src/mux/stream.rs`

- [ ] **Step 1: Write failing tests for StreamFlow**

Add a `#[cfg(test)] mod tests` block at the bottom of `src/mux/stream.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::task::{Wake, Waker};

    struct TestWaker {
        woken: std::sync::atomic::AtomicBool,
    }
    impl TestWaker {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                woken: std::sync::atomic::AtomicBool::new(false),
            })
        }
        fn was_woken(&self) -> bool {
            self.woken.load(std::sync::atomic::Ordering::Acquire)
        }
    }
    impl Wake for TestWaker {
        fn wake(self: Arc<Self>) {
            self.woken.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    #[test]
    fn stream_flow_initial_available() {
        let flow = StreamFlow::new(256 * 1024);
        assert_eq!(flow.available(), 256 * 1024);
        assert!(!flow.is_closed());
    }

    #[test]
    fn stream_flow_try_consume_basic() {
        let flow = StreamFlow::new(1000);
        let consumed = flow.try_consume(600);
        assert_eq!(consumed, 600);
        assert_eq!(flow.available(), 400);

        // Consume more than available
        let consumed2 = flow.try_consume(500);
        assert_eq!(consumed2, 400);
        assert_eq!(flow.available(), 0);
    }

    #[test]
    fn stream_flow_try_consume_returns_zero_when_empty() {
        let flow = StreamFlow::new(100);
        let _ = flow.try_consume(100);
        assert_eq!(flow.try_consume(1), 0);
    }

    #[test]
    fn stream_flow_credit_adds_window() {
        let flow = StreamFlow::new(100);
        let _ = flow.try_consume(100);
        assert_eq!(flow.available(), 0);
        flow.credit(50);
        assert_eq!(flow.available(), 50);
    }

    #[test]
    fn stream_flow_credit_wakes_writer() {
        let flow = StreamFlow::new(0);
        let test_waker = TestWaker::new();
        let waker = Waker::from(test_waker.clone());
        flow.register_waker(&waker);
        assert!(!test_waker.was_woken());

        flow.credit(100);
        assert!(test_waker.was_woken());
    }

    #[test]
    fn stream_flow_close_returns_zero_available() {
        let flow = StreamFlow::new(1000);
        flow.close();
        assert_eq!(flow.available(), 0);
        assert!(flow.is_closed());
    }

    #[test]
    fn stream_flow_close_wakes_writer() {
        let flow = StreamFlow::new(0);
        let test_waker = TestWaker::new();
        let waker = Waker::from(test_waker.clone());
        flow.register_waker(&waker);

        flow.close();
        assert!(test_waker.was_woken());
    }

    #[test]
    fn stream_flow_try_consume_returns_zero_when_closed() {
        let flow = StreamFlow::new(1000);
        flow.close();
        assert_eq!(flow.try_consume(100), 0);
    }

    #[test]
    fn stream_flow_credit_noop_when_closed() {
        let flow = StreamFlow::new(0);
        flow.close();
        flow.credit(100);
        // available still returns 0 because closed
        assert_eq!(flow.available(), 0);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib mux::stream::tests`

Expected: FAIL — `StreamFlow` not defined.

- [ ] **Step 3: Implement StreamFlow**

In `src/mux/stream.rs`, add imports at the top (after existing imports):

```rust
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::task::Waker;
use futures::task::AtomicWaker;
```

Add the `StreamFlow` struct before the `MuxStream` struct definition:

```rust
/// Per-stream flow control state shared between dispatcher and MuxStream.
/// The dispatcher credits send_window (on inbound WINDOW_UPDATE) and sets closed.
/// MuxStream deducts send_window (on poll_write) and registers write_waker.
pub struct StreamFlow {
    send_window: AtomicU32,
    closed: AtomicBool,
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

    pub fn available(&self) -> u32 {
        if self.closed.load(Ordering::Acquire) {
            return 0;
        }
        self.send_window.load(Ordering::Acquire)
    }

    pub fn credit(&self, increment: u32) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        self.send_window.fetch_add(increment, Ordering::Release);
        self.write_waker.wake();
    }

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
                Err(_) => continue,
            }
        }
    }

    pub fn register_waker(&self, waker: &Waker) {
        self.write_waker.register(waker);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.write_waker.wake();
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib mux::stream::tests`

Expected: All 8 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/mux/stream.rs
git commit -m "feat(mux): add StreamFlow struct with lock-free send_window management"
```

---

### Task 3: Change Control enum and MuxStream struct

**Files:**
- Modify: `src/mux/stream.rs`
- Modify: `src/mux/connection.rs`
- Modify: `src/mux/mod.rs`

This task changes the `Control` enum and `MuxStream` struct signatures. The code won't fully compile until Task 4 updates `connection.rs` to match, but we keep changes minimal and compilable by updating both files together.

- [ ] **Step 1: Update Control enum in stream.rs**

Replace the current `Control` enum and `StreamDataReceiver` type alias:

```rust
type StreamDataReceiver = mpsc::Receiver<Option<Bytes>>;

pub enum Control {
    AcceptStream(oneshot::Sender<Result<MuxStream>>),
    NewStream((u32, mpsc::Sender<Option<Bytes>>, Option<StreamDataReceiver>)),
    StreamData(u32, Bytes, bool),
    StreamShutdown(u32, bool),
    StreamClose(u32, bool),
    Ping,
    Close,
}
```

With:

```rust
pub type StreamDataReceiver = mpsc::UnboundedReceiver<Option<Bytes>>;

pub struct NewStreamParams {
    pub stream_id: u32,
    pub sender: mpsc::UnboundedSender<Option<Bytes>>,
    pub receiver: Option<StreamDataReceiver>,
    pub flow: Arc<StreamFlow>,
}

pub enum Control {
    AcceptStream(oneshot::Sender<Result<MuxStream>>),
    NewStream(NewStreamParams),
    StreamData(u32, Bytes, bool),
    StreamShutdown(u32, bool),
    StreamClose(u32, bool),
    WindowUpdateFromPeer(u32, u32),
    WindowUpdateToPeer(u32, u32),
    Ping,
    Close,
}
```

- [ ] **Step 2: Update MuxStream struct and constructor**

Replace the `MuxStream` struct:

```rust
pub struct MuxStream {
    id: u32,
    ev_writer: PollSender<Control>,
    inbound_reader: mpsc::UnboundedReceiver<Option<Bytes>>,
    recv_buf: Bytes,
    initial_close: bool,
    close_by_remote: bool,
    read_eof: bool,
    // --- Flow control ---
    flow: Arc<StreamFlow>,
    consumed_since_update: u32,
    window_update_threshold: u32,
}
```

Replace the constructor:

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

    pub fn id(&self) -> u32 {
        self.id
    }

    fn close_reader(&mut self) {
        self.inbound_reader.close();
    }
}
```

- [ ] **Step 3: Update AsyncRead impl with WINDOW_UPDATE emission**

Add the `maybe_send_window_update` helper method (inside `impl MuxStream`, after `close_reader`):

```rust
    fn maybe_send_window_update(&mut self, bytes_read: u32) {
        self.consumed_since_update += bytes_read;
        if self.consumed_since_update >= self.window_update_threshold {
            if let Some(sender) = self.ev_writer.get_ref() {
                match sender.try_send(
                    Control::WindowUpdateToPeer(self.id, self.consumed_since_update),
                ) {
                    Ok(()) => {
                        self.consumed_since_update = 0;
                    }
                    Err(_) => {
                        // Keep consumed_since_update — retry on next poll_read
                    }
                }
            }
        }
    }
```

In the `AsyncRead` impl, add `self.maybe_send_window_update(copy_n as u32);` calls after each successful data read:

1. After `buf.put_slice(&self.recv_buf[..copy_n]);` (drain recv_buf path) — add before `return Poll::Ready(Ok(()));`
2. After `buf.put_slice(&b[..copy_n]);` (channel read path) — add before `Poll::Ready(Ok(()))`

The updated `poll_read` body for the recv_buf drain (first block):

```rust
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
```

And in the `Some(b)` match arm, after putting data into buf:

```rust
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
                    self.maybe_send_window_update(copy_n as u32);
                    Poll::Ready(Ok(()))
                }
```

- [ ] **Step 4: Update AsyncWrite impl with send_window gating**

Replace the `poll_write` method:

```rust
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

        // 2. Reserve control channel slot
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

        // 3. Consume send_window
        let allowed = self.flow.try_consume(buf.len());
        if allowed == 0 {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stream closed",
            )));
        }

        // 4. Send data
        let data = Bytes::copy_from_slice(&buf[..allowed]);
        match self.ev_writer.send_item(Control::StreamData(self.id, data, false)) {
            Ok(()) => Poll::Ready(Ok(allowed)),
            Err(ex) => Poll::Ready(Err(utils::make_io_error(&ex.to_string()))),
        }
    }
```

- [ ] **Step 5: Update mod.rs exports**

Replace `src/mux/mod.rs` contents entirely (this removes the old `DEFAULT_STREAM_CHANNEL_SIZE` export):

```rust
mod connection;
pub mod event;
mod stream;

pub use connection::Connection;
pub use connection::Mode;
pub use connection::INITIAL_STREAM_WINDOW;
pub use stream::MuxStream;
```

- [ ] **Step 6: Do not compile yet — Task 4 updates connection.rs to match**

This step is intentionally left as-is. The code won't compile until `connection.rs` is updated in Task 4.

---

### Task 4: Update connection.rs dispatcher

**Files:**
- Modify: `src/mux/connection.rs`

- [ ] **Step 1: Update imports and constants**

Replace the top of `src/mux/connection.rs`:

```rust
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
pub const WINDOW_UPDATE_THRESHOLD: u32 = INITIAL_STREAM_WINDOW / 2;
pub const CONTROL_CHANNEL_CAPACITY: usize = 256;

/// Per-stream state owned exclusively by the dispatcher.
struct StreamEntry {
    sender: mpsc::UnboundedSender<Option<Bytes>>,
    recv_window: u32,
    flow: Arc<StreamFlow>,
}
```

- [ ] **Step 2: Update Connection struct and constructor**

Replace the `Connection` struct and `new_with_stream_channel_size`:

```rust
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
```

- [ ] **Step 3: Update handle_mux_connection — read_connection_fut**

Replace the `handle_mux_connection` function signature and `read_connection_fut`:

```rust
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
```

- [ ] **Step 4: Update handle_mux_connection — read_ctrl_fut**

Replace the `read_ctrl_fut` block:

```rust
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
                    }
                }
                Control::StreamData(sid, data, incoming) => {
                    if incoming {
                        if let Some(entry) = stream_entries.get_mut(&sid) {
                            let data_len = data.len() as u32;
                            if data_len > entry.recv_window {
                                tracing::error!(
                                    "[{}/{}] flow control violation: {} > recv_window {}",
                                    conn_id, sid, data_len, entry.recv_window
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
                                        conn_id, sid
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
                        entry.recv_window = entry.recv_window
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

        // Close all streams
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
```

- [ ] **Step 5: Verify mux module compiles**

Run: `cargo check -p rsnova 2>&1 | head -40`

Expected: Errors only from callers in `tunnel/` and `main.rs` (due to renamed constructor). The mux module itself should be clean.

- [ ] **Step 6: Commit**

```bash
git add src/mux/
git commit -m "feat(mux): unbounded channels + flow control dispatcher

- Replace bounded per-stream channels with unbounded (eliminates HOL blocking)
- Add StreamEntry with recv_window tracking
- Add WindowUpdateFromPeer/WindowUpdateToPeer handling
- Call flow.close() on stream close for immediate writer notification"
```

---

### Task 5: Caller migration (tunnel + main.rs)

**Files:**
- Modify: `src/tunnel/tls_client.rs`
- Modify: `src/tunnel/tls_remote.rs`
- Modify: `src/tunnel/mod.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Update tls_client.rs**

In `src/tunnel/tls_client.rs`:

1. Rename the field in `TlsConnection` struct:
   - `pub(crate) stream_channel_size: usize` → `pub(crate) stream_window: u32`

2. In `set_connection` or `connect` method where `mux::Connection::new_with_stream_channel_size(...)` is called, rename to:
   ```rust
   mux::Connection::new_with_stream_window(r, w, mux::Mode::Client, self.id, self.stream_window)
   ```

3. In `MuxClient::from(...)` and `new_tls_client(...)` function signatures, rename parameter:
   - `stream_channel_size: usize` → `stream_window: u32`

4. In struct initialization: `stream_channel_size` → `stream_window`

- [ ] **Step 2: Update tls_remote.rs**

In `src/tunnel/tls_remote.rs`:

1. In `start_tls_remote_server(...)` and `handle_tls_connection(...)` function signatures:
   - `stream_channel_size: usize` → `stream_window: u32`

2. In the call to `mux::Connection::new_with_stream_channel_size(...)`:
   ```rust
   mux::Connection::new_with_stream_window(r, w, mux::Mode::Server, id, stream_window)
   ```

- [ ] **Step 3: Update tunnel/mod.rs**

In `src/tunnel/mod.rs`, rename `stream_channel_size: usize` → `stream_window: u32` in `start_tunnel_client` and any related forwarding functions.

- [ ] **Step 4: Update main.rs CLI arg**

In `src/main.rs`, in the `Args` struct:

Replace:
```rust
/// Per-stream mux inbound channel size (TLS protocol only)
#[default(mux::DEFAULT_STREAM_CHANNEL_SIZE)]
#[arg(long)]
mux_stream_channel_size: usize,
```

With:
```rust
/// Per-stream flow control window in bytes (TLS protocol only)
#[default(mux::INITIAL_STREAM_WINDOW)]
#[arg(long)]
mux_stream_window: u32,
```

Update all references from `args.mux_stream_channel_size` → `args.mux_stream_window`.

- [ ] **Step 5: Verify full build compiles**

Run: `cargo build`

Expected: SUCCESS (no errors).

- [ ] **Step 6: Run existing tests**

Run: `cargo test`

Expected: All existing tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/tunnel/tls_client.rs src/tunnel/tls_remote.rs src/tunnel/mod.rs src/main.rs
git commit -m "refactor: rename stream_channel_size → stream_window across callers

CLI arg: --mux-stream-channel-size → --mux-stream-window
Default: 16 (items) → 262144 (bytes, 256KB)"
```

---

### Task 6: Run full test suite and verify

**Files:**
- No new files

- [ ] **Step 1: Run all tests**

Run: `cargo test`

Expected: All tests pass.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-features`

Expected: No errors (warnings acceptable).

- [ ] **Step 3: Run fmt check**

Run: `cargo fmt --check`

Expected: No formatting issues (fix if any).

- [ ] **Step 4: Verify build with QUIC feature**

Run: `cargo build --features s2n_quic`

Expected: SUCCESS — QUIC path unchanged.

- [ ] **Step 5: Final commit (if any fmt fixes needed)**

```bash
git add -u
git commit -m "style: cargo fmt"
```

---

## Execution Notes

- **Tasks 1-4 are the core changes** — they must be landed together for correctness (unbounded without flow control = OOM risk). The intermediate commits are for reviewer clarity but the branch should be merged as a unit.
- **Task 5** is purely mechanical renaming.
- **Task 6** is verification only.
- Keep `use futures::SinkExt;` in `stream.rs` — it's still needed for `poll_flush_unpin` in the unchanged `poll_flush` impl. Keep `use futures::ready;` — it's still used in `poll_flush` and `poll_shutdown`.
