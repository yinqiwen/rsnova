# Tunnel Mode Multi-Connection Support

**Date:** 2026-06-05
**Status:** Approved for implementation

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
start_tunnel_client_tls → for i in 0..concurrent { spawn(run_tunnel_connection_tls) }
```

Each spawned task is fully independent:
- Own reconnection loop with exponential backoff
- Own `TlsConnection` instance
- Own `accept_stream` loop handling reverse streams
- Each authenticates independently with the same `client_id` and `tunnel_entries`
- All tasks share the same `CancellationToken` for config reload

The `--concurrent` arg (already in `Args`, default 5) is threaded into `start_tunnel_client_tls` as a parameter. The same pattern applies to the QUIC tunnel client path.

### Server-Side Registry (`src/tunnel/tunnel_registry.rs`)

**Current:** `client_id -> single connection`

**New:** `client_id -> Vec<ConnectionEntry>` where `ConnectionEntry` holds the connection and metadata.

Registration logic:
- If `client_id` is new: create entry with tunnel entries + first connection
- If `client_id` already exists: replace entire entry (drop old connections, use new entries and new connection). This handles config reload races cleanly.

Reverse stream routing:
- Look up the vec by `client_id`
- Round-robin cursor advances modulo vec length
- Try to open stream on the selected connection; if dead, skip to next
- If all connections dead, return error
- Remove dead connections from vec; remove `client_id` entry if vec becomes empty

This mirrors how `MuxClient::open_stream` works on the client proxy side.

### Authentication (`src/tunnel/tunnel_remote.rs`)

Each connection sends its own `AuthRequest::Register` with the same `client_id` and `tunnel_entries`. The server validates port ranges (existing logic) and calls the updated registry method.

On config reload: client-side `CancellationToken` drops all connections, then new connections arrive with potentially different tunnel entries. The "replace entire entry" semantics handle this — first new registration replaces, subsequent ones append.

### Files to Change

1. **`src/tunnel/tunnel_client.rs`** — Thread `concurrent` param, spawn N tasks
2. **`src/tunnel/tunnel_registry.rs`** — Vec-based storage, round-robin, dead connection cleanup
3. **`src/tunnel/tunnel_remote.rs`** — Update register call for new registry API
4. **`src/tunnel/mod.rs`** — Pass `concurrent` through to tunnel client start functions
5. **`src/main.rs`** — Pass `args.concurrent` to `start_tunnel_client`

No changes to mux layer, transport layer, or wire protocol.

### Failure Modes

- **Single connection dies:** Other connections unaffected. Dead task reconnects independently via its own backoff loop.
- **All connections dead:** Server returns error on reverse stream open. Client tasks reconnect independently.
- **Config reload:** CancellationToken drops all N connections. Server sees N drops, then N new registrations. Replace-on-first-registration handles the transition.
- **Server restart:** All client connections fail. Each task reconnects independently with backoff.

### Out of Scope

- Dynamic connection scaling (add/remove connections at runtime based on load)
- Connection health-based stream weighting
- QUIC-specific multi-connection (same pattern applies but `S2NQuicConnection` has separate code path)
