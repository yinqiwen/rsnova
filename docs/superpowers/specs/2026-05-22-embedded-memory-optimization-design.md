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

| Constant | File | Old | New Default | Item Type | Item Size | Configurable Via |
|----------|------|-----|-------------|-----------|-----------|------------------|
| `PROXY_CHANNEL_CAPACITY` | `tunnel/client.rs` | 256 (hardcoded) | 32 | `Message` (enum) | ~120 B | `--proxy-channel-capacity` |
| `CONTROL_CHANNEL_CAPACITY` | `mux/connection.rs` | 256 (hardcoded) | 64 | `Control` (enum) | ~48 B | `--control-channel-capacity` |
| `DEFAULT_STREAM_CHANNEL_SIZE` | `mux/connection.rs` | 16 | 4 | `Option<Bytes>` | ~24 B | `--mux-stream-channel-size` (existing) |

Note: Channel memory = capacity × item_size + allocator overhead. The `Message` enum is dominated by `OpenStreamRequest` (contains `Option<TcpStream>`, `Option<UdpServerStream>`, `OpenStreamEvent` with two `String` fields, `Option<Vec<u8>>`). The `Control` enum is dominated by `NewStream` (contains `u32` + `mpsc::Sender` + `Option<StreamDataReceiver>`).

Implementation: Add `proxy_channel_capacity` and `control_channel_capacity` fields to `Args` struct. Thread these values through `tunnel/mod.rs::start_tunnel_client()` → `tunnel_client.rs` → `tls_client.rs` / `s2n_quic_client.rs` channel creation sites.

### 2. Buffer Size Reductions

| Location | File | Old | New | Notes |
|----------|------|-----|-----|-------|
| Transfer buffer | `tunnel/stream.rs:60` | `[0u8; 8192]` | `[0u8; 4096]` | Per active connection ×2 (bidirectional); see note below |
| SNI peek buf (peek_sni) | `tunnel/tls_local.rs:244` | `vec![0; 4096]` | `[0u8; 2048]` stack | Avoids heap alloc; 2048B covers TLS 1.3 + multi-extension ClientHello |
| SNI peek buf (peek_sni_v2) | `tunnel/tls_local.rs:124` | `vec![0u8; 4096]` | `[0u8; 2048]` stack | Same rationale |
| Admin server read | `admin.rs:27` | `vec![0u8; 8192]` | `[0u8; 1024]` stack | Admin requests are tiny |
| HTTP header read | `tunnel/http_local.rs:19` | `[0; 4096]` | `[0; 2048]` | Proxy HTTP headers typically <1KB |
| BufReader capacity | `mux/connection.rs:99` | default 8KB | `BufReader::with_capacity(2048, r)` | Event header is 8B; 2KB sufficient |

**SNI buffer rationale**: TLS 1.3 ClientHellos with multiple ALPN protocols, extended key shares, and long SNI names can exceed 1024B. Using 2048B (half the original 4096B) provides a comfortable margin while still saving 2KB per buffer. If the peek buffer is exhausted, the connection gracefully degrades to transparent proxy mode. **Must emit a `warn!` log when peek fails due to insufficient buffer**, so the issue is diagnosable in the field.

**Transfer buffer note**: Reducing from 8KB to 4KB doubles the number of read/write syscalls per transfer. Before shipping, run an iperf-through-proxy benchmark on the target embedded device. If throughput drops >20%, consider keeping 8KB with lazy allocation (allocate on first use, free after idle timeout) instead of a static per-connection buffer.

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

**Stack size validation**: 256KB must accommodate the deepest call chain in the runtime — notably rustls TLS handshake and s2n-quic connection setup. Before release, run the full test suite with `RUST_MIN_STACK=262144` in release mode, specifically testing:
- QUIC + TLS simultaneous handshake paths
- Deeply nested async task spawning under load
- If any stack overflow occurs, bump to 512KB (still 50% saving over 1MB)

### 4. Allocator — mimalloc Made Optional

Current: `mimalloc` forced on non-ARM platforms.

New: `mimalloc` gated behind optional feature `mimalloc_alloc`.

**Cargo.toml**:
```toml
[features]
default = []
mimalloc_alloc = ["mimalloc"]

[target.'cfg(not(target_arch = "arm"))'.dependencies]
mimalloc = { version = "0.1", optional = true }
```

**main.rs**:
```rust
#[cfg(feature = "mimalloc_alloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

Effect: System allocator returns pages to OS more eagerly, keeping RSS closer to actual usage. High-throughput deployments can opt-in via `--features mimalloc_alloc`.

### 5. Reduce Data Copies

**5a. Cap `poll_write` allocation size** (`mux/stream.rs:138`):

```rust
let write_len = buf.len().min(self.max_write_size);
let data = Bytes::copy_from_slice(&buf[..write_len]);
let ctrl = Control::StreamData(self.id, data, false);
// Return write_len (not buf.len()) to caller
```

Default `max_write_size` = 4096, configurable via `--mux-max-write-size` CLI arg.

Prevents oversized single allocations. Callers will loop on partial writes (standard AsyncWrite contract).

Trade-off: A write larger than `max_write_size` will produce multiple channel messages instead of one, increasing scheduler overhead. At <50 concurrent connections this is acceptable; high-throughput deployments should raise the cap (e.g., `--mux-max-write-size 65536`).

**5b. Avoid zeroed allocation in `read_event`** (`mux/event.rs:291`):

```rust
// Old: BytesMut::zeroed(body_data_len as usize)
// New: allocate uninitialized buffer since read_exact overwrites immediately
let mut dbuf = BytesMut::with_capacity(body_data_len as usize);
// SAFETY: `read_exact` below writes exactly `body_data_len` bytes before any
// read of `dbuf` occurs. If `read_exact` returns Err, `dbuf` is dropped without
// being read. No uninitialized memory is ever observed.
unsafe {
    dbuf.set_len(body_data_len as usize);
}
reader.read_exact(&mut dbuf[..]).await?;
```

Alternative (safer but marginally slower): Use `ReadBuf` with `MaybeUninit` to avoid the `unsafe` block entirely:

```rust
let mut dbuf = BytesMut::zeroed(body_data_len as usize);
reader.read_exact(&mut dbuf[..]).await?;
```

Decision: Use the `unsafe` version with the SAFETY comment. The zero-fill cost is negligible for small bodies (<4KB) but measurable for large event payloads at high throughput. If future refactoring introduces early returns between `set_len` and `read_exact`, the SAFETY comment will flag the invariant violation during review.

### 6. CLI Parameter Additions

New fields in `Args` struct (`main.rs`):

```rust
#[default(32)]
#[arg(long)]
proxy_channel_capacity: usize,

#[default(64)]
#[arg(long)]
control_channel_capacity: usize,

#[default(4096)]
#[arg(long)]
mux_max_write_size: usize,
```

These flow through the call chain:
- `proxy_channel_capacity` → `tunnel/mod.rs` → `tls_client.rs`, `s2n_quic_client.rs` channel creation
- `control_channel_capacity` → `tunnel/mod.rs` → `mux::Connection::new_with_stream_channel_size` (add parameter)
- `mux_max_write_size` → `mux::MuxStream` (stored as field, used in `poll_write`)

Unchanged defaults (per user decision):
- `--max-connections`: 256
- `--threads`: 2
- `--concurrent`: 5

Existing parameter with changed default:
- `--thread-stack-size`: existing CLI arg (default was 1048576), new default 262144 (see Change 3)

---

## Files Affected

1. `Cargo.toml` — mimalloc optional, new feature (`mimalloc_alloc`)
2. `src/main.rs` — Args fields (+`proxy_channel_capacity`, `control_channel_capacity`, `mux_max_write_size`), runtime builder, allocator cfg
3. `src/mux/connection.rs` — channel capacities parameterized, BufReader capacity
4. `src/mux/stream.rs` — poll_write cap (configurable `max_write_size` field)
5. `src/mux/event.rs` — read_event allocation with SAFETY comment
6. `src/tunnel/mod.rs` — `start_tunnel_client()` signature extended with new capacity params
7. `src/tunnel/client.rs` — `PROXY_CHANNEL_CAPACITY` removed as constant, accept as parameter
8. `src/tunnel/tunnel_client.rs` — thread capacity params to connection setup
9. `src/tunnel/stream.rs` — transfer buf size
10. `src/tunnel/tls_local.rs` — peek buf stack allocation (2048B)
11. `src/tunnel/tls_client.rs` — pass channel capacity
12. `src/tunnel/s2n_quic_client.rs` — pass channel capacity
13. `src/tunnel/http_local.rs` — header buf size
14. `src/admin.rs` — admin read buf size

---

## Memory Savings Estimate (50 concurrent connections)

| Change | Per-Unit Saving | Units | Total Saving |
|--------|----------------|-------|-------------|
| PROXY_CHANNEL_CAPACITY 256→32 | ~26.9 KB (224 slots × ~120B per `Message`) | 2 connections | ~53.8 KB |
| CONTROL_CHANNEL_CAPACITY 256→64 | ~9.2 KB (192 slots × ~48B per `Control`) | 2 connections | ~18.4 KB |
| DEFAULT_STREAM_CHANNEL_SIZE 16→4 | ~288 B (12 slots × ~24B per `Option<Bytes>`) | 50 streams × 2 dir | ~28.1 KB |
| Transfer buffer 8KB→4KB | 4 KB | 50 × 2 (bidir) | ~400 KB |
| SNI peek heap→stack 4KB→2KB | 4 KB heap avoided | 2 functions | ~8 KB |
| Admin buf 8KB→1KB stack | 7 KB heap avoided | 1 | ~7 KB |
| HTTP header 4KB→2KB | 2 KB | per request | ~2 KB |
| BufReader 8KB→2KB | 6 KB | 2 connections | ~12 KB |
| Thread stack 1MB→256KB | 768 KB | 2 threads | ~1.5 MB |
| mimalloc removed | ~50 KB initial heap | 1 | ~50 KB |
| poll_write cap 4KB | caps peak alloc | per stream | prevents spikes |
| read_event uninit | avoids zero-fill | per event | marginal (CPU only) |

**Estimated total: ~2.1 MB direct saving**, plus RSS improvement from system allocator returning pages more eagerly and reduced peak allocation from poll_write cap.

Note: PROXY_CHANNEL_CAPACITY applies per `MuxClient` instance (one per tunnel connection, typically 2 for redundancy), not per proxied connection. The 50 proxied connections share the same channel.

## Testing

- `cargo test` — all existing tests must pass
- `cargo build` (no features) — compiles without mimalloc
- `cargo build --features mimalloc_alloc` — compiles with mimalloc
- `RUST_MIN_STACK=262144 cargo test --features s2n_quic` — verify no stack overflow with reduced thread stack, especially TLS/QUIC handshake paths
- Manual: verify RSS with `--threads 1` on a test workload stays under 20MB for 50 connections

### Additional Test Cases

- **Channel backpressure**: Test behavior when `proxy_channel_capacity` (32) is saturated — verify no message loss, only backpressure delay
- **SNI peek boundary**: Craft a TLS ClientHello that is exactly 2048B — verify SNI extraction succeeds at the boundary
- **SNI peek overflow**: Craft a ClientHello >2048B — verify graceful fallback to transparent proxy with `warn!` log emitted
- **poll_write partial**: Write a buffer >4096B through `MuxStream` — verify correct reassembly on the receiving end (multiple partial writes coalesced)
- **Throughput regression**: iperf3 through proxy at 50 connections — document throughput delta vs. 8KB buffer baseline

## Backward Compatibility

All changes are default-value shifts. Users can restore original behavior via CLI args or config file:

```toml
proxy_channel_capacity = 256
control_channel_capacity = 256
mux_stream_channel_size = 16
mux_max_write_size = 65536
threads = 4
thread_stack_size = 1048576
```

For high-throughput deployments, build with `--features mimalloc_alloc` to restore mimalloc.
