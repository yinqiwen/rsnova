# Tunnel Mode Multi-Connection Support

**Date:** 2026-06-05
**Updated:** 2026-06-06
**Status:** Approved for implementation — server-side multi-connection already done; client-side `concurrent` threading pending
**Last reviewed:** 2026-06-06 (review round 2)

## Problem

Tunnel mode (NAT traversal / reverse tunnel) creates exactly one TLS/QUIC connection between client and server. This limits throughput and provides no fault tolerance — a single connection failure drops all active reverse tunnels until reconnection completes.

Proxy mode already supports multiple connections via `--concurrent` (default 5) using `MuxClient` with round-robin stream distribution. Tunnel mode should support the same.

## Design

### Approach

Spawn N independent tunnel client tasks, each with its own connection, reconnection loop, and authentication. Server-side registry tracks multiple connections per `client_id` and round-robins reverse streams across them.

### Client Side (`src/tunnel/tunnel_client.rs`)

**Current flow:**
```
start_tunnel_client_tls → loop { run_tunnel_connection_tls → accept_stream loop }
```

**New flow:**
```
start_tunnel_client_tls → for i in 0..concurrent { spawn(tunnel_client_loop_tls) }
```

Each spawned task is fully independent:
- Own reconnection loop with exponential backoff
- Own `TlsConnection` instance
- Own `accept_stream` loop handling reverse streams
- Each authenticates independently with the same `client_id` and `tunnel_entries`
- All tasks share the same `CancellationToken` for config reload
- Each uses `max_age_secs` with per-connection jitter via `retirement_jitter_secs(next_tunnel_conn_seed())` for staggered retirement (monotonic atomic counter, not `conn_index`, ensuring unique jitter even across reconnections)

The `--concurrent` arg (already in `Args`, default 5) is threaded into `start_tunnel_client_tls` as a parameter. The same pattern applies to the QUIC tunnel client path.

**Note:** `max_age_secs` (from `--connection-max-age`) is already threaded through the call chain (`main.rs` → `mod.rs` → `tunnel_client.rs`). The multi-connection change only needs to add the `concurrent` parameter.

### Server-Side Registry (`src/tunnel/tunnel_registry.rs`) — ALREADY IMPLEMENTED

**Current state:** Already supports `client_id -> Vec<ClientConnection>` with round-robin routing.

Key structures (already in code):
- `ClientState` (line 45): `connections: Vec<ClientConnection>`, `cursor: usize`
- `ClientConnection` (line 40): `handler: ConnectionHandler`, `conn_id: u32`
- `ConnectionHandler` (line 35): enum abstracting TLS (`Arc<mux::Connection>`) vs QUIC (`s2n_quic::connection::Handle`)
- `next_connection()` (line 53): round-robin cursor modulo vec length

Registration logic (current, additive):
- `register_route()` (line 113): appends route entries, creates `ClientState` if new
- `add_connection()` (line 175): pushes connection to client's vec
- `remove_connection()` (line 165): removes by `conn_id`, returns true if vec empty
- `remove_client_routes()` (line 146): removes all routes for a client, returns empty ports

**Design decision:** Registration is additive rather than "replace entire entry". This works because:
1. On config reload, client-side `CancellationToken` drops all N connections
2. Server detects each connection drop via `remove_connection()`
3. Client reconnects with new config, sends N fresh registrations
4. Each registration adds incrementally to the existing (now empty) vec

No changes needed here for multi-connection support.

### Authentication (`src/tunnel/tunnel_remote.rs`)

Each connection sends its own `AuthRequest::Register` with the same `client_id` and `tunnel_entries`. The server validates port ranges (existing logic) and calls `register_route()` + `add_connection()`.

On config reload: client-side `CancellationToken` drops all N connections, server removes them via `remove_connection()`, then client reconnects with new config and sends N fresh registrations. The additive registration semantics work correctly here because the connection vec is empty by the time new registrations arrive.

### Known Bug: `max_age_secs = 0` in Tunnel Mode

Both `run_tunnel_connection_tls` (`tunnel_client.rs:124`) and `run_quic_tunnel_connection` (`s2n_quic_client.rs:384`) unconditionally compute a retirement timer:

```rust
let retire_at = Instant::now() + Duration::from_secs((max_age_secs as i64 + jitter).max(0) as u64);
```

When `max_age_secs = 0` (user intends "never retire"), jitter can be negative (range [-100, +100]), so `(0 + jitter).max(0) = 0` → `retire_at = Instant::now()`, causing immediate retirement and infinite reconnect loops.

Proxy mode avoids this correctly: `client.rs:251` checks `self.max_age_secs > 0` before triggering retirement in `health_check()`.

**Fix (must be done alongside multi-connection changes):**

```rust
if max_age_secs > 0 {
    let seed = next_tunnel_conn_seed() as usize;
    let jitter = retirement_jitter_secs(seed);
    let retire_at = Instant::now() + Duration::from_secs((max_age_secs as i64 + jitter).max(0) as u64);
    tokio::select! {
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(retire_at)) => {
            // drain handles, return Ok(())
        }
        result = conn.accept_stream() => { /* handle */ }
    }
} else {
    // No retirement — accept streams forever
    loop {
        let result = conn.accept_stream().await;
        // handle result (spawn task or break on error)
    }
}
```

This applies to both TLS and QUIC tunnel paths.

### Proxy vs Tunnel `concurrent` Semantics

Proxy mode and tunnel mode use `--concurrent` differently:

| Aspect | Proxy Mode | Tunnel Mode |
|--------|-----------|-------------|
| Structure | Single `MuxClient` with managed connection pool | N independent spawned tasks |
| Connection selection | Round-robin across pool via `cursor` | Each task owns exactly one connection |
| Health checking | Centralized `health_check()` pings + reconnects | Per-task reconnection loop with own backoff |
| Retirement | `PoolConnection` wrapper with max-age + jitter | Per-task `sleep_until(retire_at)` in `select!` |
| Stream routing | `MuxClient::open_stream()` picks best connection | Server-side `next_connection()` round-robins |

Both achieve the same goal (N connections for throughput + fault tolerance) but with different architectures. The tunnel approach has better fault isolation (one task crashing doesn't affect others) but less centralized control.

### Files to Change

**Already done (no further changes needed):**
- `src/tunnel/tunnel_registry.rs` — Vec-based storage, round-robin, dead connection cleanup (all implemented)
- `src/tunnel/tunnel_remote.rs` — Registration uses `register_route()` + `add_connection()` (already correct)
- `max_age_secs` threading — `main.rs` → `mod.rs` → `tunnel_client.rs` (already threaded)

**Still needed:**
1. **`src/tunnel/tunnel_client.rs`** — Add `concurrent` param, spawn N tasks with `tunnel_client_loop_tls` extracted. The extracted function inherits the current outer loop's exponential backoff (1s → 60s, reset on 30s+ connection) and CancellationToken-based config reload. Also fix `max_age_secs = 0` bug (see Known Bug section above).
2. **`src/tunnel/s2n_quic_client.rs`** — Same pattern for QUIC tunnel client. Also fix `max_age_secs = 0` bug (same fix as TLS). Note: QUIC path does not accept `stream_window` (QUIC handles flow control natively), so the `concurrent` addition is the only parameter change.
3. **`src/tunnel/mod.rs`** — Add `concurrent: usize` param to `start_tunnel_client`, pass through to both branches
4. **`src/main.rs`** — Pass `args.concurrent` to `start_tunnel_client` (currently only passes `connection_max_age`)

No changes to mux layer, transport layer, or wire protocol.

### Notes

**QUIC vs TLS `stream_window` asymmetry:** `start_tunnel_client_tls` accepts `stream_window: u32` while `start_tunnel_client_quic` does not. This is intentional — QUIC handles flow control at the protocol level, so `mux::Connection` (which uses `stream_window`) is not used for QUIC. The `MuxConnection` trait's `active_stream_count()` returns 0 for QUIC for the same reason. Not a bug, but worth knowing when comparing the two paths.

**`tokio::select!` and `accept_stream` cancellation:** The current `tokio::select!` in the retirement loop calls `conn.accept_stream()` as one branch. When the retirement timer fires, tokio drops the `accept_stream` future. For TLS this is safe (the underlying `mux::Connection::accept_stream` is channel-based and cancellation-tolerant). For QUIC, `accept_bidirectional_stream` is also cancellation-safe. However, this relies on implementation details — a future refactor to use a `CancellationToken`-wrapped pattern would be more explicit. Not blocking for this change.

### Failure Modes

- **Single connection dies:** Other connections unaffected. Dead task reconnects independently via its own backoff loop.
- **All connections dead:** Server returns error on reverse stream open. Client tasks reconnect independently.
- **Config reload:** CancellationToken drops all N connections. Server removes each via `remove_connection()`. Client reconnects with new config, sends N fresh registrations that add to the now-empty vec.
- **Server restart:** All client connections fail. Each task reconnects independently with backoff.

### Out of Scope

- Dynamic connection scaling (add/remove connections at runtime based on load)
- Connection health-based stream weighting

**Note:** QUIC multi-connection is in scope — `s2n_quic_client.rs` uses the same pattern as TLS (`start_tunnel_client_quic` needs the same `concurrent` parameter addition).

### Alternative Considered: Reusing `MuxClient` for Tunnel Mode

`MuxClient` already provides multi-connection management with round-robin `open_stream()`, `health_check()`, `add_connection()`, and `retirement_notify`. Tunnel mode could reuse it instead of spawning N independent tasks:

- Client-side: one `MuxClient` holding N connections, `open_stream()` round-robins
- Server-side: already uses `next_connection()` round-robin (compatible)

**Why the "N independent tasks" approach was chosen:**
- **Fault isolation:** One task crashing or stalling doesn't affect others. With `MuxClient`, a slow `open_stream()` blocks the single `mux_client_loop` channel.
- **Simplicity:** Each task is a self-contained reconnection loop — no shared state, no channel coordination.
- **Tunnel vs proxy semantics:** Proxy mode needs `open_stream()` from a local listener (requests arrive on the channel). Tunnel mode needs `accept_stream()` from the remote server (reverse streams arrive on the connection). `MuxClient` is designed around the former pattern.

**Trade-off:** Less code reuse, but the tunnel connection lifecycle (accept reverse streams, max-age retirement, config reload) is fundamentally different from the proxy lifecycle (open streams on demand, health check, pool management). Forcing both through `MuxClient` would add complexity to an already complex abstraction.
