# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

rsnova is a Rust-based proxy/tunnel application providing secure, multiplexed network tunneling over QUIC and TLS protocols. It supports HTTP, SOCKS5, TLS SNI, and transparent proxies with connection pooling, stream multiplexing, NAT traversal (reverse tunnels), and health checking.

## Build Commands

```bash
# Development build (default features: none; QUIC is optional)
cargo build

# Build with QUIC support
cargo build --features s2n_quic

# Release build
cargo build --release --features s2n_quic

# Cross-compile for Linux musl targets (uses `cross`)
./ci/build_linux.sh x86_64-unknown-linux-musl

# Build for macOS/Windows (uses native cargo)
./ci/build_other.sh aarch64-apple-darwin
```

**Toolchain:** Rust stable. No nightly features required.

## Testing

```bash
# Run all tests
cargo test

# Run a specific test by name
cargo test test_parse_single_port

# Run tests in a specific module
cargo test tunnel::tunnel_config::tests
```

Tests exist in `src/mux/event.rs` and `src/tunnel/tunnel_config.rs`. There are no integration tests or a `tests/` directory.

## Linting

No custom clippy or rustfmt configuration exists. Use standard:

```bash
cargo clippy --all-features
cargo fmt --check
```

## Architecture

### Data Flow

```
[Client] → Local Proxy (protocol detection via peek) → MuxClient pool → Encrypted Tunnel (TLS/QUIC) → Server demux → Target
```

1. **Protocol detection** (`tunnel/local.rs`): Peeks first 3 bytes of each inbound connection. Routes to SOCKS5 (`0x05`), TLS SNI (`0x16-0x18`), HTTP (ASCII method prefix), or transparent proxy (fallback).
2. **Protocol handlers** extract the target address and send an `OpenStreamRequest` to the `MuxClient` via a bounded `mpsc` channel (`PROXY_CHANNEL_CAPACITY = 256`).
3. **MuxClient** (`tunnel/client.rs`): Round-robin connection pool. Opens a mux stream on the next valid connection; health-checks reconnect invalid ones.
4. **Mux layer** (`mux/`): Binary frame protocol over any `AsyncRead + AsyncWrite`. 8-byte header encodes `flag_len` (flag in low byte, length in upper 24 bits) + `stream_id`. Flags: OPEN, FIN, SYN, DATA, PING, SHUTDOWN, AUTH, AUTH_ACK, REVERSE_OPEN.
5. **Transport** (TLS via `tokio-rustls` or QUIC via `s2n-quic`): Encrypts the mux byte stream to the remote server.
6. **Server-side** demultiplexes streams, connects to target, and relays bidirectionally with idle timeout.

### Key Traits

- **`MuxConnection`** (`tunnel/client.rs:88`): Transport-agnostic connection. Methods: `ping`, `connect`, `open_stream`, `accept_stream`, `is_valid`, `set_connection`. Implemented by `TlsMuxConnection` and `QuicMuxConnection`.
- **`MuxClientTrait`** (`tunnel/client.rs:99`): Connection pool interface. `open_stream` round-robins across connections; `health_check` reconnects dead ones.

### Mux Internals

- `mux::Connection` (`mux/connection.rs`): Spawns a task that reads/writes events on the underlying stream. Client uses even stream IDs (seed 0), server uses odd (seed 1).
- `mux::MuxStream` (`mux/stream.rs`): Per-stream `AsyncRead`/`AsyncWrite` backed by `mpsc` channels. Configurable channel size (`--mux-stream-channel-size`, default 16).

### NAT Traversal / Reverse Tunnel

- **Client** (`tunnel/tunnel_client.rs`): Connects to server, authenticates with `FLAG_AUTH` (sends client_id + tunnel entries), waits for `FLAG_REVERSE_OPEN` to open local connections.
- **Server** (`tunnel/tunnel_remote.rs` + `tunnel/tunnel_registry.rs`): Validates port ranges, registers client tunnels, listens on allocated ports, forwards inbound connections back through the mux.
- **Config** (`tunnel/tunnel_config.rs`): Parses `--tunnel` args (supports IPv4, IPv6, SNI) and `--tunnel-port-range`.

### Feature Flags

- `s2n_quic` (optional, not default): Enables QUIC transport via AWS s2n-quic. Gates `tunnel/s2n_quic_client.rs` and `tunnel/s2n_quic_remote.rs`.

### Platform-Specific Code

- **Linux only**: `SO_ORIGINAL_DST` for transparent TCP proxy, `IP_TRANSPARENT` + tproxy for UDP (`tunnel/transparent.rs`, `tunnel/udp_local.rs`, `utils/net.rs`).
- **Unix**: Daemonization (`utils/daemon.rs`); Windows has a no-op stub.
- **Non-ARM**: Uses `mimalloc` as global allocator; ARM falls back to system allocator with `aws-lc-rs` bindgen.

## Adding New Components

### New Transport Protocol
1. Create `src/tunnel/myprotocol_client.rs` implementing `MuxConnection`
2. Create `src/tunnel/myprotocol_remote.rs` for server-side
3. Add modules to `src/tunnel/mod.rs`
4. Add scheme match arm in `service_main()` (`src/main.rs`)

### New Local Protocol
1. Create `src/tunnel/myproto_local.rs`
2. Add module to `src/tunnel/mod.rs`
3. Add detection logic to `handle_local_tunnel()` in `tunnel/local.rs` (match on peeked bytes)

## Configuration

All CLI args are also valid TOML keys in a config file (`-c config.toml`). CLI takes precedence. Key struct is `Args` in `main.rs` using `clap-serde-derive`.
