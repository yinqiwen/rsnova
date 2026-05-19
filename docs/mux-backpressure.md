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
