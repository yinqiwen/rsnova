# rsnova

A Rust-based secure proxy/tunnel providing multiplexed network tunneling over QUIC and TLS protocols.

## Features

- **QUIC/TLS Transport** — Encrypted tunnel over QUIC (s2n-quic) or native TLS (rustls)
- **Multi-Protocol Proxy** — Auto-detects protocol from first bytes: SOCKS5 (`0x05`), TLS ClientHello (`0x16`), HTTP methods → falls back to transparent proxy
- **HTTP & HTTPS Proxy** — Plain HTTP proxy and HTTP CONNECT (HTTPS) proxy
- **TLS SNI Proxy** — Extracts SNI from TLS ClientHello for routing
- **Stream Multiplexing** — Multiple streams over a single connection with backpressure control
- **Connection Pooling** — Configurable pool of concurrent connections with health checking and auto-reconnection
- **NAT Traversal** — Reverse tunnel mode to expose local services behind NAT, supports SNI-based routing
- **Admin Server** — HTTP endpoint for `/metrics` monitoring
- **TOML Config** — Full configuration via config file or CLI args (CLI takes precedence)
- **Daemon Mode** — Background execution on Unix (`-d`)
- **Transparent Proxy** — Linux `SO_ORIGINAL_DST` / tproxy support for both TCP and UDP
- **Self-Signed Cert** — Built-in certificate generation via `--rcgen`
- **Log Rotation** — Daily log file rotation with `--log`

## Build

```sh
cargo build --release
```

QUIC support (optional feature):

```sh
cargo build --release --features s2n_quic
```

Cross-compile for Linux musl (requires [cross](https://github.com/cross-rs/cross)):

```sh
./ci/build_linux.sh x86_64-unknown-linux-musl
./ci/build_linux.sh arm-unknown-linux-musleabi
./ci/build_linux.sh arm-unknown-linux-musleabihf
```

Build for macOS/Windows:

```sh
./ci/build_other.sh x86_64-apple-darwin
./ci/build_other.sh aarch64-apple-darwin
./ci/build_other.sh x86_64-pc-windows-msvc
```

## Quick Start

### 1. Generate TLS Certificate

```sh
./target/release/rsnova --rcgen --tls_host mydomain.io
```

This generates `cert.pem` and `key.pem` in the current directory.

### 2. Start Server

```sh
# TLS protocol
./target/release/rsnova --role server --protocol tls --key key.pem --cert cert.pem --listen 0.0.0.0:48100

# QUIC protocol (requires --features s2n_quic)
./target/release/rsnova --role server --protocol quic --key key.pem --cert cert.pem --listen 0.0.0.0:48100
```

### 3. Start Client

```sh
# Connect via TLS
./target/release/rsnova --role client --cert cert.pem --listen 127.0.0.1:48100 --remote tls://<server-ip>:48100 --tls_host mydomain.io

# Connect via QUIC
./target/release/rsnova --role client --cert cert.pem --listen 127.0.0.1:48100 --remote quic://<server-ip>:48100 --tls_host mydomain.io
```

### 4. Use Proxy

Configure your browser or tools to use `socks5://127.0.0.1:48100` or `http://127.0.0.1:48100` as the proxy.

## NAT Traversal (Reverse Tunnel)

Expose local services behind NAT by running the client in tunnel mode:

```sh
# Client: expose local SSH (port 22) as remote port 2222
./target/release/rsnova --role client --cert cert.pem --remote tls://<server-ip>:48100 \
  --tls_host mydomain.io --tunnel-client-id myhost \
  --tunnel 22:2222

# Server: allow tunnel connections on ports 8000-9000
./target/release/rsnova --role server --protocol tls --key key.pem --cert cert.pem \
  --listen 0.0.0.0:48100 --tunnel-port-range 8000-9000
```

Multiple tunnels and port ranges are supported:

```sh
# Client: expose multiple services
./target/release/rsnova --role client --cert cert.pem --remote tls://<server-ip>:48100 \
  --tls_host mydomain.io --tunnel-client-id myhost \
  --tunnel 22:2222 --tunnel 3306:23306 --tunnel 192.168.1.100:8080:28080:api.example.com

# Server: allow multiple port ranges
./target/release/rsnova --role server --protocol tls --key key.pem --cert cert.pem \
  --listen 0.0.0.0:48100 --tunnel-port-range 8000-9000,10000-10100
```

Tunnel format:

| Format | Description |
|--------|-------------|
| `port` | localhost:port → remote same port |
| `localPort:remotePort` | localhost:localPort → remote remotePort |
| `host:localPort:remotePort` | host:localPort → remote remotePort |
| `host:localPort:remotePort:sni` | Same with SNI domain routing |
| `[ipv6]:localPort:remotePort[:sni]` | IPv6 host support |

## Configuration

All options can be specified via CLI or TOML config file (`-c config.toml`). CLI args override config file values.

```toml
listen = "127.0.0.1:48100"
protocol = "tls"
role = "client"
remote = "tls://1.2.3.4:48100"
cert = "cert.pem"
key = "key.pem"
tls_host = "mydomain.io"
concurrent = 5
threads = 2
thread_stack_size = 1048576
idle_timeout_secs = 30
mux_stream_channel_size = 16
max_connections = 256
admin_listen = "127.0.0.1:48102"
```

### Key Options

| Option | Default | Description |
|--------|---------|-------------|
| `--listen` | `127.0.0.1:48100` | Proxy listen address |
| `--role` | `client` | `client` or `server` |
| `--protocol` | `tls` | `tls` or `quic` |
| `--remote` | — | Remote server URL (`tls://host:port` or `quic://host:port`) |
| `--key` | `key.pem` | TLS private key path |
| `--cert` | `cert.pem` | TLS certificate path |
| `--tls-host` | `mydomain.io` | TLS SNI hostname |
| `--concurrent` | `5` | Number of connections in the pool |
| `--threads` | `2` | Tokio worker threads |
| `--thread-stack-size` | `1048576` | Thread stack size in bytes |
| `--idle-timeout-secs` | `30` | Connection idle timeout |
| `--mux-stream-channel-size` | `16` | Per-stream mux inbound channel size (TLS only) |
| `--max-connections` | `256` | Max concurrent proxy connections |
| `--admin-listen` | `127.0.0.1:48102` | Admin HTTP server address (`/metrics`) |
| `--tunnel` | — | Tunnel entries for NAT traversal (conflicts with `--listen` and `--tproxy`) |
| `--tunnel-client-id` | — | Client identifier for tunnel mode |
| `--tunnel-port-range` | — | Server-side allowed port ranges (e.g., `8000-9000,10000-10100`) |
| `--tproxy` | `false` | Transparent proxy mode (Linux) |
| `--rcgen` | `false` | Generate self-signed certificate |
| `--profile` | `false` | Profiling mode (conflicts with `--daemon`) |
| `-d, --daemon` | `false` | Run in background (Unix) |
| `--log` | — | Log file path (enables daily rotation) |
| `-c, --config` | — | Path to TOML config file |

## Project Structure

```
src/
├── main.rs              # CLI entry, arg parsing, runtime setup, admin server
├── mux/                 # Stream multiplexing subsystem
│   ├── connection.rs    # Mux connection: bidirectional stream multiplex
│   ├── stream.rs        # MuxStream: AsyncRead/AsyncWrite over channels
│   └── event.rs         # Binary frame protocol, serialization, auth/tunnel payloads
├── tunnel/              # Transport and protocol layer
│   ├── client.rs        # MuxClient pool, ProxySender/Receiver
│   ├── local.rs         # Protocol detection & dispatch
│   ├── socks5_local.rs  # SOCKS5 protocol handler
│   ├── http_local.rs    # HTTP/HTTPS proxy handler
│   ├── tls_local.rs     # TLS SNI extraction & proxy
│   ├── transparent.rs   # Transparent proxy (SO_ORIGINAL_DST / tproxy)
│   ├── tls_client.rs    # TLS transport client
│   ├── tls_remote.rs    # TLS server
│   ├── s2n_quic_client.rs  # QUIC transport client (feature-gated)
│   ├── s2n_quic_remote.rs  # QUIC server (feature-gated)
│   ├── tunnel_client.rs    # NAT traversal client
│   ├── tunnel_remote.rs    # NAT traversal server
│   ├── tunnel_config.rs    # Tunnel arg parsing & port range validation
│   ├── tunnel_registry.rs  # Server-side tunnel state
│   ├── stream.rs           # Bidirectional data relay with idle timeout
│   └── udp_local.rs        # UDP tproxy handler (Linux only, feature-gated)
└── utils/               # Utility modules
    ├── tls.rs           # TLS cert/key reading
    ├── net.rs           # TCP/UDP listener, SO_ORIGINAL_DST, IP_TRANSPARENT
    ├── metrics.rs       # Custom metrics recorder
    ├── io.rs            # Async I/O helpers
    ├── udp.rs           # UDP client/server stream abstractions
    ├── error.rs         # I/O error helpers
    ├── clean.rs         # Log rotation cleanup
    ├── daemon.rs        # Unix daemonization
    └── daemon_windows.rs # Windows daemon stub
```

## Data Flow

```
[Browser/Tool] ──SOCKS5/HTTP/HTTPS/TLS──▶ [Client local proxy]
                                                   │
                                         Protocol detection (peek first bytes)
                                                   │
                                         Extract target address
                                                   │
                                         MuxClient picks connection from pool
                                                   │
                                         Open mux stream over encrypted tunnel
                                                   │
                                         [TLS/QUIC encrypted tunnel to server]
                                                   │
                                         Server demux → connect to target
                                                   │
                                         Bidirectional relay
```

## License

This project is licensed under the terms found in the repository.
