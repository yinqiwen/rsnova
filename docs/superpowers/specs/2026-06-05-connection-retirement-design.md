# Connection Retirement / Rotation Design

## Overview

Add a connection max-age mechanism to the TLS/QUIC connection pool. When a connection exceeds its configured lifetime, it enters "retired" state: it rejects new proxy requests but keeps serving existing streams until they drain. Simultaneously, a replacement connection is created and added to the pool. This applies to both proxy mode (connection pool) and tunnel mode (single connection).

## Motivation

Long-lived connections can accumulate stale state, hit server-side limits, or suffer degraded performance. Periodic connection rotation improves reliability and load distribution. The random jitter prevents all connections from retiring simultaneously (thundering herd).

## Configuration

### CLI Parameter

```
--connection-max-age <SECONDS>    Max connection lifetime in seconds (default: 1800)
```

Also valid as a TOML config key: `connection_max_age = 1800`

### Random Jitter

Each connection gets a random jitter of `[-100, +100]` seconds added to the base max-age. This spreads retirement times across a 200-second window. Uses `rand::Rng::gen_range(-100..=100)`.

Actual retire time = `Instant::now() + Duration::from_secs(max_age_secs as i64 + jitter)` (jitter is signed i64, range [-100, +100])

## Architecture

### Data Structure Changes

#### `MuxClient` (src/tunnel/client.rs)

```rust
struct PoolConnection<T> {
    conn: T,
    created_at: Instant,
    retire_at: Instant,  // created_at + max_age + jitter
    retired: bool,
}

pub(crate) struct MuxClient<T> {
    pub(crate) url: url::Url,
    pub(crate) conns: Vec<PoolConnection<T>>,
    pub(crate) host: String,
    pub(crate) cursor: usize,
    pub(crate) cert: Option<PathBuf>,
    pub(crate) max_age_secs: u64,
}
```

#### New Message Variant

```rust
pub enum Message {
    OpenStream(OpenStreamRequest),
    HealthCheck,
    AddConnection(Box<dyn Any + Send + Sync>),
}
```

No new message variant needed -- retirement is triggered during `health_check` and connection replacement uses existing `AddConnection`.

### Proxy Mode Flow

```
health_check loop (every 1s):
  for each connection:
    if retired and no active streams:
      close connection, mark inner = None
    if not retired and age >= retire_at:
      mark retired = true
      create new connection (spawn task)
    if not retired and is_valid:
      ping check

open_stream:
  round-robin, skip retired connections
  if all connections are retired, return error
```

#### Connection Replacement

When `health_check` detects a connection has reached its retirement time:
1. Mark it `retired = true`
2. Spawn an async task to create a new connection (outside the client loop)
3. New task connects, authenticates (proxy mode auth), then sends `Message::AddConnection` to the client loop
4. `add_connection` places it in an empty slot (from a fully-drained retired connection that has `inner = None`) or appends

This ensures the client loop is never blocked by connection establishment.

#### Stream Drain Tracking

`mux::Connection` needs to expose an active stream count. Currently `stream_entries: HashMap<u32, StreamEntry>` lives inside the spawned dispatcher task. Add `active_stream_count: Arc<AtomicUsize>` to `Connection` struct, shared with the dispatcher task. Increment on stream open, decrement on stream close (FIN/SHUTDOWN). When a retired connection's count reaches 0, it can be safely closed and replaced.

Add a public method:
```rust
impl Connection {
    pub fn active_stream_count(&self) -> usize {
        self.active_stream_count.load(Ordering::Relaxed)
    }
}

### Tunnel Mode Flow

```
run_tunnel_connection_tls:
  created_at = Instant::now()
  retire_at = created_at + max_age + jitter
  handles = Vec<JoinHandle>

  loop:
    if now >= retire_at:
      // wait for all active reverse streams
      while !handles.is_empty():
        handles.retain(|h| !h.is_finished())
        sleep(100ms)
      return Ok(())  // triggers reconnect in outer loop

    match accept_stream:
      Ok => spawn handle_reverse_stream, push handle
      Err => return Err
```

The outer reconnect loop in `start_tunnel_client_tls` already handles reconnection with exponential backoff.

### QUIC Support

Same pattern applies to `QuicMuxConnection` and `s2n_quic_client.rs`. The `MuxClient` changes are generic over `T: MuxConnection`, so QUIC connections get retirement for free once the trait and `MuxClient` are updated.

## Files to Modify

1. **`Cargo.toml`** -- add `rand` dependency
2. **`src/main.rs`** -- add `--connection-max-age` CLI arg, pass to client/tunnel constructors
3. **`src/tunnel/client.rs`** -- `PoolConnection`, update `MuxClient` struct, update `MuxClientTrait` impl
4. **`src/tunnel/tls_client.rs`** -- pass `max_age_secs` to `MuxClient::from`, use `PoolConnection`
5. **`src/tunnel/s2n_quic_client.rs`** -- same changes for QUIC
6. **`src/tunnel/tunnel_client.rs`** -- add retirement logic to `run_tunnel_connection_tls`
7. **`src/mux/connection.rs`** -- expose active stream count method (if not already available)

## Edge Cases

- **All connections retired simultaneously**: Jitter prevents this, but if it happens, `open_stream` returns error and the caller logs + drops the proxy request
- **Connection fails before retirement**: Existing health check logic handles reconnection
- **Retirement during active stream transfer**: Stream continues until natural completion; retirement only blocks new streams
- **Tunnel mode: reverse streams outlive retirement**: The `handles` vec tracks all spawned tasks; retirement waits for all to finish before returning

## Testing

- Unit test: verify `PoolConnection` age check with mocked `Instant`
- Unit test: verify jitter range is `[-100, +100]`
- Integration: verify retired connections are skipped in `open_stream`
- Integration: verify tunnel mode reconnects after retirement
