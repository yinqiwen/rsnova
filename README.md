# rsnova

[简体中文](README.zh-CN.md)

A Rust secure proxy and tunnel that multiplexes traffic over encrypted TLS and QUIC connections.

## Features

- **Encrypted transport** — TLS (rustls) and QUIC (s2n-quic); the server listens on both simultaneously
- **Multi-protocol proxy** — Auto-detects SOCKS5, HTTP/HTTPS, and TLS SNI; falls back to transparent proxy
- **Stream multiplexing** — Many logical streams over one connection, with WINDOW_UPDATE flow control and a connection pool
- **NAT traversal** — Reverse tunnel mode to expose local services on a public server, with optional SNI routing
- **Transparent proxy** — Linux `SO_ORIGINAL_DST` / tproxy support (TCP + UDP)
- **Self-signed certs** — Built-in `--rcgen` certificate generation
- **Daemon mode** — Background execution on Unix (`-d`)
- **Log rotation** — Daily rotation via `--log`
- **Admin HTTP** — Built-in `/metrics` and `/config` endpoints

## Compatibility and Connection Lifecycle

- Current clients and servers must be upgraded together. The wire protocol now uses target-open acknowledgements and tunnel drain control and is not compatible with older releases.
- SOCKS5 and HTTP `CONNECT` success is returned only after the server has connected to the requested target; target failures are reported to the local client instead of becoming a silent reset.
- `--connection-max-age` retires connections gracefully: no new streams are assigned, active one-way or bidirectional transfers finish, and only then is the connection replaced.
- Reverse-tunnel max-age rotation and config reload register a replacement generation before draining the old generation, so existing reverse streams continue without a routing gap.

## Use Cases

| Scenario | Description |
|----------|-------------|
| Secure proxy | Client exposes SOCKS5/HTTP locally; traffic is forwarded through an encrypted tunnel to reach targets on the server side |
| NAT traversal | Expose internal services (SSH, databases, web apps) on a public host via reverse tunnels |
| Transparent proxy | Gateway transparent proxy on Linux with iptables/nftables, no client configuration |
| TLS SNI routing | Route by SNI hostname from TLS ClientHello to different backend services |

## Quick Start

### Install

```sh
cargo install rsnova
```

### 1. Generate certificates

```sh
rsnova --rcgen true --tls_host mydomain.io
```

This creates `cert.pem` and `key.pem` in the current directory.

### 2. Start the server

The server listens for both TLS (TCP) and QUIC (UDP) on the same address — no protocol flag required:

```sh
rsnova --role server --key key.pem --cert cert.pem --listen 0.0.0.0:48100
```

### 3. Start the client

The client selects transport from the `--remote` URL scheme:

```sh
# TLS
rsnova --role client --cert cert.pem --listen 127.0.0.1:48100 --remote tls://<server-ip>:48100 --tls_host mydomain.io

# QUIC
rsnova --role client --cert cert.pem --listen 127.0.0.1:48100 --remote quic://<server-ip>:48100 --tls_host mydomain.io
```

### 4. Use the proxy

Point your browser or tools at `socks5://127.0.0.1:48100` or `http://127.0.0.1:48100`.

### NAT Traversal (Reverse Tunnel)

```sh
# Client: map local SSH (22) to remote port 2222
rsnova --role client --cert cert.pem --remote tls://<server-ip>:48100 \
  --tls_host mydomain.io --tunnel-client-id myhost \
  --tunnel 22:2222

# Server: allow tunnel ports 8000-9000
rsnova --role server --key key.pem --cert cert.pem \
  --listen 0.0.0.0:48100 --tunnel-port-range 8000-9000
```

Tunnel entry formats:

| Format | Description |
|--------|-------------|
| `port` | localhost:port → same port on server |
| `localPort:remotePort` | localhost:localPort → server remotePort |
| `host:localPort:remotePort` | host:localPort → server remotePort |
| `host:localPort:remotePort:sni` | Same as above, with SNI domain routing |
| `[ipv6]:localPort:remotePort[:sni]` | IPv6 host support |

### Configuration File

All options can be set in a TOML file (`-c config.toml`). CLI arguments override file values:

```toml
listen = "127.0.0.1:48100"
role = "client"
remote = "tls://1.2.3.4:48100"
cert = "cert.pem"
key = "key.pem"
tls_host = "mydomain.io"
concurrent = 5
threads = 2
idle_timeout_secs = 120
mux_stream_window = 262144
max_connections = 256
admin_listen = "127.0.0.1:48102"
```

### Common Options

| Option | Default | Description |
|--------|---------|-------------|
| `--listen` | `127.0.0.1:48100` | Local proxy listen address |
| `--role` | `client` | `client` or `server` |
| `--remote` | — | Remote URL (`tls://host:port` or `quic://host:port`) |
| `--concurrent` | `5` | Connection pool size |
| `--max-connections` | `256` | Max concurrent proxy connections |
| `--idle-timeout-secs` | `120` | Idle timeout per relay (seconds) |
| `--mux-stream-window` | `262144` | TLS mux per-stream flow-control window (bytes) |
| `--tunnel` | — | Tunnel entries (conflicts with `--listen` / `--tproxy`) |
| `--tunnel-port-range` | — | Allowed port ranges on server, e.g. `8000-9000,10000-10100` |
| `--tproxy` | `false` | Transparent proxy mode (Linux) |
| `-d, --daemon` | `false` | Run in background (Unix) |
| `--log` | — | Log file path |
| `-c, --config` | — | TOML config file path |
