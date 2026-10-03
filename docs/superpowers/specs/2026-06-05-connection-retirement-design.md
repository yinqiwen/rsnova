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

Each connection gets a deterministic jitter derived from its connection index: `((conn_index * 73) % 201) - 100` seconds, giving a range of `[-100, +100]`. This spreads retirement times across a 200-second window with zero dependencies.

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

### Proxy Mode Flow

```
health_check loop (every 1s):
  for each connection:
    if retired and no active streams:
      close connection, mark inner = None
    if not retired and age >= retire_at:
      mark retired = true
      spawn replacement task with retry
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
4. `add_connection` places it using existing logic: replace first `!is_valid()` slot (a fully-drained retired connection has `inner = None`, so `is_valid()` returns false), or append if all slots are valid

**Replacement retry**: The spawned task retries with exponential backoff (1s, 2s, 4s, ... up to 30s) for up to 5 attempts. On each failure, it logs a warning. After 5 failed attempts, it gives up and logs an error. The retired connection will eventually drain and be cleaned up by the next `health_check` cycle. The pool may temporarily shrink — this is a known tradeoff (see Capacity Gap section).

This ensures the client loop is never blocked by connection establishment.

#### Stream Drain Tracking

Add `active_stream_count` to the `MuxConnection` trait:

```rust
pub(crate) trait MuxConnection {
    // ... existing methods ...
    fn active_stream_count(&self) -> usize;
}
```

Implementation: `mux::Connection` already tracks `stream_entries: HashMap<u32, StreamEntry>` inside its dispatcher task. Add `active_stream_count: Arc<AtomicUsize>` to `Connection` struct, shared with the dispatcher task. Increment on stream open, decrement on stream close (FIN/SHUTDOWN). `TlsConnection` and `S2NQuicConnection` delegate to their inner connection's count.

```rust
impl Connection {
    pub fn active_stream_count(&self) -> usize {
        self.active_stream_count.load(Ordering::Relaxed)
    }
}
```

### Tunnel Mode Flow (TLS)

The current `run_tunnel_connection_tls` has a blocking `accept_stream().await` that prevents retirement checks. Use `tokio::select!` to race retirement against stream acceptance:

```rust
async fn run_tunnel_connection_tls(..., max_age_secs: u64) -> Result<()> {
    let mut conn = TlsConnection::new(stream_window);
    conn.connect(url, cert_path, host).await?;

    // ... auth handshake (unchanged) ...

    let jitter = ((conn_index.wrapping_mul(73)) % 201) as i64 - 100;
    let retire_at = Instant::now() + Duration::from_secs(max_age_secs as i64 + jitter);
    let mut handles: Vec<JoinHandle<()>> = Vec::new();

    loop {
        // Remove finished handles
        handles.retain(|h| !h.is_finished());

        tokio::select! {
            _ = tokio::time::sleep_until(retire_at) => {
                tracing::info!("Tunnel connection reached max age, draining...");
                // Wait for all active reverse streams to finish
                for h in handles {
                    let _ = h.await;
                }
                return Ok(());  // triggers reconnect in outer loop
            }
            result = conn.accept_stream() => {
                let (mut stream_send, mut stream_recv) = result?;
                handles.push(tokio::spawn(async move {
                    if let Err(e) = handle_reverse_stream(&mut stream_recv, &mut stream_send, idle_timeout_secs).await {
                        tracing::warn!("Reverse stream error: {}", e);
                    }
                }));
            }
        }
    }
}
```

The outer reconnect loop in `start_tunnel_client_tls` already handles reconnection with exponential backoff. After `run_tunnel_connection_tls` returns `Ok(())` (retirement), backoff resets since the connection was productive.

### Tunnel Mode Flow (QUIC)

`run_quic_tunnel_connection` in `s2n_quic_client.rs` needs the same pattern. The current code uses `connection.accept_bidirectional_stream()` in a loop — wrap it in the same `tokio::select!` against a retirement timer:

```rust
tokio::select! {
    _ = tokio::time::sleep_until(retire_at) => {
        // drain handles, return Ok(())
    }
    result = connection.accept_bidirectional_stream() => {
        // spawn reverse stream handler
    }
}
```

### Known Tradeoffs

#### Capacity Gap During Replacement Latency

When a connection is marked retired, `open_stream` immediately starts skipping it. The replacement won't arrive for 1-2 seconds (connect + auth handshake). During that window, pool capacity is reduced by 1. With 5 connections and staggered retirement (jitter), this is unlikely to be noticeable. Documented as a known tradeoff — not a correctness issue.

#### Pool Shrink on Replacement Failure

If a replacement task exhausts its 5 retry attempts, the retired connection drains and is removed. The pool shrinks by 1 until the next `health_check` cycle detects the reduced count (this would require an additional "pool size check" in health_check, or we accept the shrink as a degraded state until the next retirement cycle naturally creates a replacement opportunity).

## Files to Modify

1. **`src/main.rs`** -- add `--connection-max-age` CLI arg, pass to client/tunnel constructors
2. **`src/tunnel/client.rs`** -- `PoolConnection`, update `MuxClient` struct, update `MuxClientTrait` impl, add `active_stream_count` to `MuxConnection` trait
3. **`src/tunnel/tls_client.rs`** -- pass `max_age_secs` to `MuxClient::from`, use `PoolConnection`, implement `active_stream_count`
4. **`src/tunnel/s2n_quic_client.rs`** -- same changes for QUIC, add retirement to `run_quic_tunnel_connection`
5. **`src/tunnel/tunnel_client.rs`** -- add retirement logic with `tokio::select!` to `run_tunnel_connection_tls`
6. **`src/mux/connection.rs`** -- expose `active_stream_count` via `Arc<AtomicUsize>`

## Edge Cases

- **All connections retired simultaneously**: Jitter prevents this, but if it happens, `open_stream` returns error and the caller logs + drops the proxy request
- **Connection fails before retirement**: Existing health check logic handles reconnection
- **Retirement during active stream transfer**: Stream continues until natural completion; retirement only blocks new streams
- **Tunnel mode: reverse streams outlife retirement**: The `handles` vec tracks all spawned tasks; retirement uses `tokio::select!` to wait for all to finish before returning

## Testing

- Unit test: verify deterministic jitter range is `[-100, +100]`
- Unit test: verify `PoolConnection` age check with mocked `Instant`
- Integration: verify retired connections are skipped in `open_stream`
- Integration: verify tunnel mode reconnects after retirement
- Integration: verify tunnel mode retirement does not block on idle connections (select! behavior)
