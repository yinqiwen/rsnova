# Embedded Memory Optimization — Design Spec

## Context

rsnova needs to run on embedded devices with 16-64MB RAM, serving both Client and Server roles with QUIC support, at <50 concurrent connections. The current defaults target high-throughput server deployments and waste memory in constrained environments.

## Goal

Reduce RSS and peak memory through parameter tuning, buffer sizing, and allocation strategy changes — without altering architecture or breaking backward compatibility.

## Constraints

- No architectural changes to mux layer
- All optimizations must be overridable via CLI/config
- Existing high-performance deployments unaffected (can restore original values)
- QUIC support retained

---

## Changes

### 1. Channel Capacity — Configurable with Lower Defaults

| Constant | File | Old | New Default | Configurable Via |
|----------|------|-----|-------------|------------------|
| `PROXY_CHANNEL_CAPACITY` | `tunnel/client.rs` | 256 (hardcoded) | 32 | `--proxy-channel-capacity` |
| `CONTROL_CHANNEL_CAPACITY` | `mux/connection.rs` | 256 (hardcoded) | 64 | `--control-channel-capacity` |
| `DEFAULT_STREAM_CHANNEL_SIZE` | `mux/connection.rs` | 16 | 4 | `--mux-stream-channel-size` (existing) |

Implementation: Add `proxy_channel_capacity` and `control_channel_capacity` fields to `Args` struct. Thread these values through to channel creation sites.

### 2. Buffer Size Reductions

| Location | File | Old | New | Notes |
|----------|------|-----|-----|-------|
| Transfer buffer | `tunnel/stream.rs:60` | `[0u8; 8192]` | `[0u8; 4096]` | Per active connection ×2 (bidirectional) |
| SNI peek buf (peek_sni) | `tunnel/tls_local.rs:244` | `vec![0; 4096]` | `[0u8; 1024]` stack | Avoids heap alloc; SNI within first 512B typically |
| SNI peek buf (peek_sni_v2) | `tunnel/tls_local.rs:124` | `vec![0u8; 4096]` | `[0u8; 1024]` stack | Same rationale |
| Admin server read | `main.rs:189` | `[0u8; 4096]` | `[0u8; 1024]` | Admin requests are tiny |
| HTTP header read | `tunnel/http_local.rs:19` | `[0; 4096]` | `[0; 2048]` | Proxy HTTP headers typically <1KB |
| BufReader capacity | `mux/connection.rs:99` | default 8KB | `BufReader::with_capacity(2048, r)` | Event header is 8B; 2KB sufficient |

Risk: SNI peek at 1024B may miss extremely long ClientHellos (rare Java clients). Existing error handling gracefully degrades to transparent proxy mode.

### 3. Tokio Runtime Optimization

- When `threads <= 1`, use `tokio::runtime::Builder::new_current_thread()` instead of `new_multi_thread()`
- Change `thread_stack_size` default from 1048576 (1MB) to 262144 (256KB)
- `threads` default remains 2 (unchanged per user request)

```rust
let runtime = if args.threads <= 1 {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
} else {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.threads)
        .enable_all()
        .thread_stack_size(args.thread_stack_size)
        .build()
        .expect("failed to build tokio runtime")
};
```

### 4. Allocator — mimalloc Made Optional

Current: `mimalloc` forced on non-ARM platforms.

New: `mimalloc` gated behind optional feature `mimalloc_alloc`.

**Cargo.toml**:
```toml
[features]
default = []
mimalloc_alloc = ["mimalloc"]

[target.'cfg(not(target_arch = "arm"))'.dependencies]
mimalloc = { version = "*", optional = true }
```

**main.rs**:
```rust
#[cfg(feature = "mimalloc_alloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

Effect: System allocator returns pages to OS more eagerly, keeping RSS closer to actual usage. High-throughput deployments can opt-in via `--features mimalloc_alloc`.

### 5. Reduce Data Copies

**5a. Cap `poll_write` allocation size** (`mux/stream.rs:155`):

```rust
let write_len = buf.len().min(4096);
let data = Bytes::copy_from_slice(&buf[..write_len]);
let ctrl = Control::StreamData(self.id, data, false);
// Return write_len (not buf.len()) to caller
```

Prevents oversized single allocations. Callers will loop on partial writes (standard AsyncWrite contract).

**5b. Avoid zeroed allocation in `read_event`** (`mux/event.rs:291`):

```rust
// Old: BytesMut::zeroed(body_data_len as usize)
// New: allocate without zero-fill since read_exact overwrites immediately
let mut dbuf = BytesMut::with_capacity(body_data_len as usize);
dbuf.resize(body_data_len as usize, 0);
```

### 6. CLI Parameter Additions

New fields in `Args` struct (`main.rs`):

```rust
#[default(32)]
#[arg(long)]
proxy_channel_capacity: usize,

#[default(64)]
#[arg(long)]
control_channel_capacity: usize,
```

These flow through the call chain:
- `proxy_channel_capacity` → `tls_client.rs`, `s2n_quic_client.rs` channel creation
- `control_channel_capacity` → `mux::Connection::new_with_stream_channel_size` (add parameter)

Unchanged defaults (per user decision):
- `--max-connections`: 256
- `--threads`: 2
- `--concurrent`: 5

---

## Files Affected

1. `Cargo.toml` — mimalloc optional, new feature
2. `src/main.rs` — Args fields, runtime builder, allocator cfg, admin buf
3. `src/mux/connection.rs` — channel capacities parameterized, BufReader capacity
4. `src/mux/stream.rs` — poll_write cap
5. `src/mux/event.rs` — read_event allocation
6. `src/tunnel/client.rs` — PROXY_CHANNEL_CAPACITY parameterized
7. `src/tunnel/stream.rs` — transfer buf size
8. `src/tunnel/tls_local.rs` — peek buf stack allocation
9. `src/tunnel/tls_client.rs` — pass channel capacity
10. `src/tunnel/s2n_quic_client.rs` — pass channel capacity
11. `src/tunnel/http_local.rs` — header buf size

## Testing

- `cargo test` — all existing tests must pass
- `cargo build` (no features) — compiles without mimalloc
- `cargo build --features mimalloc_alloc` — compiles with mimalloc
- Manual: verify RSS with `--threads 1` on a test workload stays under 20MB for 50 connections

## Backward Compatibility

All changes are default-value shifts. Users can restore original behavior via CLI args or config file:

```toml
proxy_channel_capacity = 256
control_channel_capacity = 256
mux_stream_channel_size = 16
threads = 4
thread_stack_size = 1048576
```
