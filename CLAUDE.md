# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

rsnova is a Rust-based proxy/tunnel application providing secure, multiplexed network tunneling over QUIC and TLS protocols. It supports HTTP and SOCKS5 proxies with connection pooling, stream multiplexing, and health checking.

## Build Commands

```bash
# Development build
cargo build

# Release build
cargo build --release

# Cross-compile for Linux targets (uses cross)
./ci/build_linux.sh

# Build for macOS/Windows
./ci/build_other.sh
```

**Note:** Uses Rust nightly toolchain (rust-toolchain.toml).

## Running

```bash
# Generate certificates
./target/release/rsnova --rcgen --tls_host mydomain.io

# Server mode (QUIC)
./target/release/rsnova --role server --protocol quic --key key.pem --cert cert.pem --listen 0.0.0.0:48100

# Client mode
./target/release/rsnova --role client --cert cert.pem --listen 127.0.0.1:48100 --remote quic://<ip:port> --tls_host mydomain.io
```

## Architecture

### Module Structure

```
src/
├── main.rs              # CLI entry, argument parsing, runtime setup
├── tunnel/              # Transport layer
│   ├── local.rs         # Protocol detection (SOCKS5/HTTP/TLS/transparent)
│   ├── socks5_local.rs  # SOCKS5 protocol handler
│   ├── http_local.rs    # HTTP/HTTPS proxy handler
│   ├── client.rs        # Generic client interface and multiplexing
│   ├── tls_client.rs    # TLS transport client
│   ├── s2n_quic_client.rs   # AWS s2n-quic client
│   └── *_remote.rs      # Server-side transport implementations
├── mux/                 # Stream multiplexing
│   ├── connection.rs    # Manages bidirectional stream multiplex
│   ├── stream.rs        # AsyncRead/AsyncWrite stream implementation
│   └── event.rs         # Binary frame protocol (SYN, FIN, RST, DATA flags)
└── utils/               # TLS, networking, metrics, UDP utilities
```

### Data Flow

1. **Local proxy** accepts connections, peeks first bytes to detect protocol (0x05=SOCKS5, 0x16-0x18=TLS, HTTP methods=HTTP)
2. Protocol handler extracts target address, sends `Message::OpenStream` to tunnel
3. **Multiplexer** assigns stream to connection pool, creates send/recv channels
4. **Transport layer** (TLS/QUIC) encrypts and sends to remote
5. **Remote server** demultiplexes, connects to target, relays data

### Key Abstractions

- **MuxConnection trait**: Transport-agnostic connection interface (`ping`, `open_stream`, `connect`)
- **MuxClientTrait**: Connection pool management with health checking
- **Message enum**: Async communication (`OpenStream`, `HealthCheck`, `AddConnection`)
- **Event protocol**: Binary frames with stream ID and flags for multiplexing

### Feature Flags

- `default = ["s2n_quic"]` - AWS s2n QUIC implementation

### Platform-Specific Code

- Linux: `SO_ORIGINAL_DST` for transparent proxy, tproxy UDP support
- Conditional compilation with `#[cfg(target_os = "linux")]` and `cfg_if`

## Adding New Components

### New Transport Protocol
1. Create `src/tunnel/myprotocol_client.rs` implementing `MuxConnection`
2. Create `src/tunnel/myprotocol_remote.rs` for server-side
3. Add module to `src/tunnel/mod.rs`
4. Add protocol handler in `main.rs` `service_main()`

### New Local Protocol
1. Create `src/tunnel/myproto_local.rs`
2. Add module to `src/tunnel/mod.rs`
3. Add detection logic to `handle_local_tunnel()` in `local.rs`
