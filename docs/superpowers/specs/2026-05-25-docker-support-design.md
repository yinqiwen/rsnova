# Docker Support Design

## Overview

Add Docker support to rsnova with a single `docker-compose.yml` using profiles to run different roles (server, client_proxy, client_tunnel). Include an interactive `--init-dir` command that generates certificates and role-specific configuration files.

## Changes Summary

| File | Change |
|---|---|
| `Dockerfile` | New: multi-stage Alpine build |
| `docker-compose.yml` | New: profiles-based compose |
| `.dockerignore` | Already exists, no change |
| `src/main.rs` | Add `--init-dir` subcommand; refactor `rcgen()` to accept output path parameter |

Note: `s2n-quic` is already an unconditional dependency (not feature-gated). No `Cargo.toml` changes needed.

## Dockerfile

Multi-stage build targeting Alpine (musl). s2n-quic + musl compatibility verified.

```dockerfile
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev cmake perl clang-dev pkgconf
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM alpine:3.21
RUN apk add --no-cache ca-certificates
COPY --from=builder /app/target/release/rsnova /usr/local/bin/rsnova
ENTRYPOINT ["rsnova"]
```

## docker-compose.yml

Uses Docker Compose profiles. Running `docker compose up` without `--profile` starts nothing (safe default).

```yaml
services:
  server:
    profiles: [server]
    build: .
    restart: unless-stopped
    ports:
      - "48100:48100"
      - "48102:48102"
    # network_mode: host  # uncomment to use host networking
    volumes:
      - ./rsnova_data:/data:ro
    command: ["-c", "/data/server.toml"]

  proxy:
    profiles: [client_proxy]
    build: .
    restart: unless-stopped
    ports:
      - "48101:48101"
    # network_mode: host
    volumes:
      - ./rsnova_data:/data:ro
    command: ["-c", "/data/client_proxy.toml"]

  tunnel:
    profiles: [client_tunnel]
    build: .
    restart: unless-stopped
    # Tunnel mode requires exposing a range of ports. Docker bridge mode
    # performs very poorly with large port ranges (1000+ mappings).
    # Use network_mode: host for production deployments.
    network_mode: host
    # ports:
    #   - "8000-9000:8000-9000"  # only if not using host network
    volumes:
      - ./rsnova_data:/data:ro
    command: ["-c", "/data/client_tunnel.toml"]
```

## `--init-dir` Command

### Invocation

Uses `--init-dir <PATH>` (named argument, not positional) to avoid ambiguity with other flags:

```bash
# Default: output to current directory
rsnova --init-dir .

# Specify output directory
rsnova --init-dir /data

# Docker usage
docker run --rm -it -v ./rsnova_data:/data rsnova --init-dir /data
```

### Preserves `--rcgen`

The existing `--rcgen` flag is preserved for backward compatibility. Internally, the `rcgen()` function is refactored to accept an output directory parameter (currently hardcoded to `./`). `--init-dir` reuses this refactored function.

### Existing File Handling

If `cert.pem`, `key.pem`, or the config file already exists in the output directory:

- **Interactive (TTY)**: For certificates, if `cert.pem` and `key.pem` already exist, prompt early: "Existing certificates found. Reuse them? [Y/n]" — if yes, skip certificate generation entirely (useful when setting up multiple roles sharing the same cert). For config files, prompt before overwriting.
- **Non-interactive (no TTY)**: Skip existing files by default. Use `--force` flag to overwrite without prompting.

```bash
# Interactive — prompts before overwriting
docker run --rm -it -v ./rsnova_data:/data rsnova --init-dir /data

# Non-interactive — skip existing files
docker run --rm -v ./rsnova_data:/data rsnova --init-dir /data

# Non-interactive — force overwrite
docker run --rm -v ./rsnova_data:/data rsnova --init-dir /data --force
```

### Interactive Flow

```
[1/N] Select role:
  1) server
  2) proxy (client)
  3) tunnel (client)
> _
```

Then asks role-specific questions:

#### Server Questions

| # | Question | Default |
|---|---|---|
| 2 | TLS hostname (for certificate generation only) | mydomain.io |
| 3 | Listen address | 0.0.0.0:48100 |
| 4 | Admin listen address | 0.0.0.0:48102 |
| 5 | Tunnel port range | 8000-9000 |

#### Client Proxy Questions

| # | Question | Default |
|---|---|---|
| 2 | TLS hostname (for cert + runtime SNI) | mydomain.io |
| 3 | Remote server address | tls://server:48100 |
| 4 | Listen address | 0.0.0.0:48101 |
| 5 | Admin listen address | 0.0.0.0:48102 |

#### Client Tunnel Questions

| # | Question | Default |
|---|---|---|
| 2 | TLS hostname (for cert + runtime SNI) | mydomain.io |
| 3 | Remote server address | tls://server:48100 |
| 4 | Tunnel client ID | my-client |
| 5 | Tunnel port mappings | 8080:80 |
| 6 | Admin listen address | 0.0.0.0:48102 |

### Output

All files written to the specified directory:

- `cert.pem` — self-signed TLS certificate
- `key.pem` — private key
- `server.toml` or `client_proxy.toml` or `client_tunnel.toml` — role-specific config

### TLS Hostname Handling

- **Server**: TLS hostname is used only for certificate generation. Not written to `server.toml`.
- **Client (proxy/tunnel)**: TLS hostname is used for certificate generation AND written to config file (needed at runtime for SNI verification).

## Configuration File Format

**Important**: TOML keys must use snake_case (matching Rust struct field names). `clap-serde-derive` does NOT support kebab-case in TOML — verified by testing that kebab-case keys are silently ignored.

### server.toml

```toml
# === Server Configuration ===
role = "server"
listen = "0.0.0.0:48100"
admin_listen = "0.0.0.0:48102"
cert = "/data/cert.pem"
key = "/data/key.pem"
tunnel_port_range = "8000-9000"

# === Optional (uncomment to customize) ===
# concurrent = 5
# threads = 2
# idle_timeout_secs = 120
# mux_stream_window = 262144
# max_connections = 256
# log = ""
```

### client_proxy.toml

```toml
# === Client Proxy Configuration ===
role = "client"
remote = "tls://server:48100"
listen = "0.0.0.0:48101"
admin_listen = "0.0.0.0:48102"
cert = "/data/cert.pem"
tls_host = "example.com"

# === Optional (uncomment to customize) ===
# concurrent = 5
# threads = 2
# idle_timeout_secs = 120
# mux_stream_window = 262144
# max_connections = 256
# log = ""
```

### client_tunnel.toml

Note: `listen` is intentionally omitted — it conflicts with `tunnel` in the Args struct (`conflicts_with_all = ["listen", "tproxy"]`). Including both would cause a parse error.

```toml
# === Client Tunnel Configuration ===
role = "client"
remote = "tls://server:48100"
admin_listen = "0.0.0.0:48102"
cert = "/data/cert.pem"
tls_host = "example.com"
tunnel_client_id = "my-client"
tunnel = ["8080:80", "8443:443"]

# === Optional (uncomment to customize) ===
# threads = 2
# idle_timeout_secs = 120
# mux_stream_window = 262144
# log = ""
```

## Usage Flow

```bash
# 1. Build image
docker compose build

# 2. Initialize (interactive, generates certs + config)
docker run --rm -it -v ./rsnova_data:/data rsnova --init-dir /data

# 3. Edit config if needed
vim rsnova_data/server.toml

# 4. Start a role
docker compose --profile server up -d
docker compose --profile client_proxy up -d
docker compose --profile client_tunnel up -d

# 5. Restart after config change (no rebuild needed)
docker compose --profile server restart

# 6. Run multiple roles together (local testing)
docker compose --profile server --profile client_proxy up -d

# 7. Set up a second role (reuses existing certs)
docker run --rm -it -v ./rsnova_data:/data rsnova --init-dir /data
# → "Existing certificates found. Reuse them? [Y/n]" → Y
```

## Design Decisions

1. **Profiles over separate services** — `docker compose up` without profile starts nothing (safe). Explicit activation required.
2. **Interactive init over static templates** — Generates only what's needed for the chosen role. Reduces user confusion.
3. **TOML config over env vars / command args** — Config file can be edited and reloaded with `restart`, no container rebuild.
4. **`--init-dir` complements `--rcgen`** — `--init-dir` is the new interactive entry point for initialization (certs + config). `--rcgen` is preserved for backward compatibility. Uses named argument (not positional) to avoid clap parsing ambiguity.
5. **Alpine only** — s2n-quic + musl compatibility verified. No Debian fallback needed.
6. **QUIC included** — `s2n-quic` is already an unconditional dependency, no feature flag changes needed.
7. **Flat data directory** — All generated files (certs + config) live at the root of the data volume, no subdirectories.
8. **snake_case in TOML** — Verified that `clap-serde-derive` only recognizes snake_case keys. kebab-case is silently ignored.
9. **No healthcheck** — For single-machine compose, `restart: unless-stopped` handles crash recovery. Healthcheck is only useful for Swarm/orchestrated deployments with `depends_on.condition: service_healthy`. Omitting it avoids admin port conflicts when running multiple roles and keeps the runtime image smaller (no wget needed).
10. **Tunnel mode recommends host networking** — Large port range mappings (8000-9000) perform poorly in Docker bridge mode. Compose file uses `network_mode: host` by default for tunnel service.
