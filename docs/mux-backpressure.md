# Mux backpressure and per-stream queue stability

## Problem

The TLS mux implementation multiplexes multiple logical streams over one TLS connection. Each logical stream receives inbound DATA through a bounded per-stream channel.

When concurrent downloads create bursts, a single slow stream can fill its per-stream inbound channel. If the mux dispatcher awaits that channel, delivery for other mux events can be delayed. In observed benchmark failures this showed up as:

```text
transfer finish: timeout after inactive 30051ms, last active at +1005ms
No stream:3/10 found ... data len:8192
download closed early
```

This means one stream hit idle timeout and was removed, while DATA for the same stream arrived later.

## Short-term mitigation

The stream channel size is configurable:

```text
--mux-stream-channel-size <N>
```

Default: `16`.

Trade-offs observed in local benchmarks:

- `16`: best single-stream throughput, stable in the latest 3-run test.
- `32`: best small-packet latency in tests, but had an observed concurrent download failure.
- `64`: more stable for concurrent bursts, with higher memory usage and sometimes lower single-stream throughput.

For high-concurrency download workloads, increase the value, for example:

```bash
rsnova --protocol tls --mux-stream-channel-size 64 ...
```

## Concrete fix: try_send + per-stream overflow buffer

Replace the blocking `stream_sender.send(Some(data)).await` in the dispatcher with non-blocking `try_send()` plus a bounded per-stream overflow buffer. This eliminates head-of-line blocking while tolerating short bursts.

### Problem locations in `src/mux/connection.rs`

| Line | Code | Risk |
|------|------|------|
| 169 | `e.get().send(Some(Bytes::new())).await` | Duplicate stream ID error path blocks dispatcher (rare but possible) |
| 191 | `stream_sender.send(Some(data)).await` | DATA dispatch blocks dispatcher when channel full |
| 236 | `sender.send(Some(Bytes::new())).await` | StreamShutdown EOF marker blocks dispatcher |
| 249 | `sender.send(None).await` | StreamClose notification blocks dispatcher |

> 行 169 的重复 stream ID 路径虽然极少触发，但在理论上也是阻塞点。修复时应一并改为 `try_send`。

### Implementation

Add a per-stream backlog map alongside the existing `stream_senders`:

```rust
let mut stream_backlogs: HashMap<u32, VecDeque<Bytes>> = HashMap::new();
const MAX_BACKLOG_BYTES: usize = 256 * 1024; // 256KB per stream
```

Replace the blocking DATA dispatch (line 191):

```rust
Control::StreamData(sid, data, incoming) => {
    match stream_senders.get(&sid) {
        Some(stream_sender) => {
            if incoming {
                // First, try to drain any existing backlog for this stream
                if let Some(backlog) = stream_backlogs.get_mut(&sid) {
                    while let Some(front) = backlog.front() {
                        match stream_sender.try_send(Some(front.clone())) {
                            Ok(()) => { backlog.pop_front(); }
                            Err(_) => break,
                        }
                    }
                    if backlog.is_empty() {
                        stream_backlogs.remove(&sid);
                    }
                }

                // Now try to send the current data
                match stream_sender.try_send(Some(data)) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(Some(data))) => {
                        let backlog = stream_backlogs.entry(sid).or_default();
                        let backlog_size: usize = backlog.iter().map(|b| b.len()).sum();
                        if backlog_size + data.len() > MAX_BACKLOG_BYTES {
                            tracing::warn!(
                                "[{}/{}] stream backlog exceeded {}B, closing slow stream",
                                conn_id, sid, MAX_BACKLOG_BYTES
                            );
                            stream_senders.remove(&sid);
                            stream_backlogs.remove(&sid);
                            metrics::decrement_gauge!("mux.streams", 1.0);
                            let ev = event::new_fin_event(sid);
                            let _ = event::write_event(&mut w, ev).await;
                        } else {
                            backlog.push_back(data);
                        }
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => unreachable!(),
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        tracing::error!("[{}/{}] stream channel closed", conn_id, sid);
                        stream_senders.remove(&sid);
                        stream_backlogs.remove(&sid);
                        metrics::decrement_gauge!("mux.streams", 1.0);
                    }
                }
            } else {
                // outbound write — unchanged
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
                    "No stream:{}/{} found for data incoming:{} with data len:{}",
                    conn_id, sid, incoming, data.len()
                );
            }
        }
    }
}
```

Similarly fix NewStream duplicate ID path (line 169), StreamShutdown and StreamClose to use `try_send`:

```rust
Control::NewStream((sid, sender, receiver)) => match stream_senders.entry(sid) {
    Entry::Occupied(_e) => {
        tracing::error!("Duplicate stream id:{}", sid);
        let _ = sender.try_send(Some(Bytes::new()));  // non-blocking
    }
    // ... Vacant branch unchanged ...
}

```rust
Control::StreamShutdown(sid, remote) => {
    if let Some(sender) = stream_senders.get(&sid) {
        if !remote {
            let ev = event::new_shutdown_event(sid);
            if let Err(e) = event::write_event(&mut w, ev).await {
                tracing::error!("write shutdown failed:{}", e);
                break;
            }
        } else {
            // Non-blocking: EOF marker is zero-length, skip backlog
            let _ = sender.try_send(Some(Bytes::new()));
        }
    }
}

Control::StreamClose(sid, remote) => {
    match stream_senders.remove_entry(&sid) {
        Some((_, sender)) => {
            stream_backlogs.remove(&sid);
            metrics::decrement_gauge!("mux.streams", 1.0);
            if !remote {
                let ev = event::new_fin_event(sid);
                let _ = event::write_event(&mut w, ev).await;
            } else {
                let _ = sender.try_send(None);
            }
        }
        None => {}
    }
}
```

### Periodic backlog drain

At the end of each dispatcher loop iteration, drain pending backlogs opportunistically:

```rust
// After the main match block, drain backlogs
let mut drained_streams = Vec::new();
for (sid, backlog) in stream_backlogs.iter_mut() {
    if let Some(sender) = stream_senders.get(sid) {
        while let Some(front) = backlog.front() {
            match sender.try_send(Some(front.clone())) {
                Ok(()) => { backlog.pop_front(); }
                Err(_) => break,
            }
        }
    }
    if backlog.is_empty() {
        drained_streams.push(*sid);
    }
}
for sid in drained_streams {
    stream_backlogs.remove(&sid);
}
```

### Trade-offs

| Aspect | Value |
|--------|-------|
| Max memory per stream | 256KB (configurable via `MAX_BACKLOG_BYTES`) |
| Dispatcher blocking | Eliminated — all sends are non-blocking |
| Burst tolerance | Absorbs up to 256KB burst per stream before closing |
| Data ordering | Preserved — backlog is FIFO per stream |
| Stream fate on overload | Closed with FIN, only the slow stream affected |

> **关于溢出关闭策略**：当前方案在 backlog 超过 `MAX_BACKLOG_BYTES` 时直接关闭 stream 并发 FIN。
> 这对于慢消费 stream 是可接受的折中，但 256KB 阈值偏保守——一个 TCP 接收窗口就可能达到 64KB-1MB。
> 未来可考虑按时间判断（backlog 中最老数据的停留时间超过阈值才关闭），而非纯字节数。
> 在流控（WINDOW_UPDATE）实现之前，关闭慢 stream 是防止内存无限增长的唯一手段。

### Configurable constants

Consider exposing these as CLI args or config alongside `--mux-stream-channel-size`:

- `MAX_BACKLOG_BYTES` (default 256KB): per-stream overflow limit before stream is closed
- `stream_channel_size` (default 16): existing bounded channel capacity

---

## Architectural direction

Queue size tuning is a mitigation, not a complete fix. The core architectural rule should be:

> Backpressure from one logical stream must not block the whole mux connection dispatcher.

### Preferred long-term fix: flow control

Implement mux-level flow control:

- Add or enable a `WINDOW_UPDATE` event.
- Maintain per-stream send and receive windows.
- Send DATA only when the peer window has capacity.
- Increase the peer window as the application reads data.
- Optionally add a connection-level memory cap.

This makes a slow stream consume only its own window instead of blocking unrelated streams.

### Medium-term fix: avoid awaiting per-stream send in the global dispatcher

Replace direct awaited delivery:

```rust
stream_sender.send(Some(data)).await
```

with a non-blocking dispatch strategy:

- Try `try_send` first.
- If full, enqueue into a per-stream backlog.
- Give each backlog a byte/item limit.
- If a stream exceeds its limit, close only that slow stream instead of blocking the whole mux connection.
- Preserve in-order delivery per stream.

This reduces head-of-line blocking in the mux dispatcher, but still needs memory limits to avoid unbounded buffering.

### Better scheduler shape

A more robust TLS mux architecture would separate responsibilities:

```text
MuxConnection
├── reader task
│   └── decode frames -> stream states
├── writer task
│   └── fair scheduling of outbound frames
├── stream table
│   └── stream_id -> StreamState
└── flow control
    ├── per-stream window
    └── connection-level memory cap
```

Each `StreamState` should own:

- inbound buffer/backlog
- outbound buffer/backlog
- close/shutdown state
- read/write wakers
- flow-control window counters

## QUIC note

This issue mainly applies to the custom TLS mux path. QUIC already provides native stream-level and connection-level flow control, so QUIC is the better fit for heavy concurrent download workloads when available.
