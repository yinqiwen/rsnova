# Docker Support Design

**Status: Phase 1 implemented (Phase 2 pending)**

## Overview

Add Docker support to rsnova with a single `docker-compose.yml` using profiles to run different roles (server, client_proxy, client_tunnel). Include an `--init-dir` command that generates certificates and role-specific configuration files.

## Prerequisites

- A GitHub Release must exist with the corresponding tag (created by CI via `.github/workflows/build-release.yml`)
- Phase 1 targets **amd64 only** (`x86_64-unknown-linux-musl`)
- Docker `TARGETARCH=arm` refers to 32-bit ARM (`arm-unknown-linux-musleabihf`), distinct from arm64/aarch64
- **aarch64 (arm64) is not yet supported** — CI does not build `aarch64-unknown-linux-musl`; added in Phase 2
- For frequent CI builds, prefer pinning `VERSION=vX.Y.Z` to avoid GitHub API rate limits on the `latest` resolution

## Changes Summary

| File | Change |
|---|---|
| `Dockerfile` | New: download release binary from GitHub |
| `docker-compose.yml` | New: profiles-based compose |
| `.dockerignore` | Update: remove stale references to deleted files |
| `src/main.rs` | Add `--init-dir` and `--force` flags; refactor `rcgen()` to accept output path parameter |
| `.github/workflows/build-release.yml` | Add `aarch64-unknown-linux-musl` target (Phase 2) |

Note: `s2n-quic` is already an unconditional dependency (not feature-gated). No `Cargo.toml` changes needed.

## Dockerfile

Downloads pre-built binary from GitHub Releases. Phase 1 supports amd64 only; arm64 support added after CI update.

```dockerfile
FROM alpine:3.21

ARG VERSION=latest
ARG TARGETARCH

SHELL ["/bin/sh", "-euo", "pipefail", "-c"]

RUN apk add --no-cache ca-certificates wget \
    && case "${TARGETARCH}" in \
         amd64) TARGET="x86_64-unknown-linux-musl" ;; \
         arm)   TARGET="arm-unknown-linux-musleabihf" ;; \
         *)     echo "ERROR: Unsupported architecture: ${TARGETARCH}. Only amd64 and 32-bit arm are supported. arm64/aarch64 support planned for Phase 2." >&2; exit 1 ;; \
       esac \
    && if [ "${VERSION}" = "latest" ]; then \
         VERSION_TAG=$(wget -qO- "https://api.github.com/repos/yinqiwen/rsnova/releases/latest" \
           | grep '"tag_name"' | head -1 | cut -d '"' -f 4) \
         && [ -n "${VERSION_TAG}" ] || { echo "ERROR: Failed to resolve latest release tag. Check network or use VERSION=vX.Y.Z to avoid API calls." >&2; exit 1; }; \
       else \
         VERSION_TAG="${VERSION}"; \
       fi \
    && NUMERIC_VERSION="${VERSION_TAG#v}" \
    && DOWNLOAD_URL="https://github.com/yinqiwen/rsnova/releases/download/${VERSION_TAG}/rsnova-${NUMERIC_VERSION}-${TARGET}.tar.gz" \
    && echo "Downloading: ${DOWNLOAD_URL}" \
    && wget -qO /tmp/rsnova.tar.gz "${DOWNLOAD_URL}" \
    || { echo "ERROR: Download failed. Verify release ${VERSION_TAG} exists and contains ${TARGET} artifact." >&2; exit 1; } \
    && tar xzf /tmp/rsnova.tar.gz -C /usr/local/bin/ \
    && rm /tmp/rsnova.tar.gz \
    && chmod +x /usr/local/bin/rsnova

ENTRYPOINT ["rsnova"]
```

Build examples:
```bash
# Latest release (amd64)
docker compose build

# Specific version (recommended for CI / reproducible builds)
docker compose build --build-arg VERSION=v0.1.0
```

## docker-compose.yml

Uses Docker Compose profiles. Running `docker compose up` without `--profile` starts nothing (safe default).

**Admin port allocation**: Each role uses a different admin port to allow multi-role deployment on one host without conflicts.

```yaml
x-rsnova-common: &rsnova-common
  image: rsnova:${RSNOVA_IMAGE_TAG:-local}
  build:
    context: .
    args:
      VERSION: ${RSNOVA_RELEASE_VERSION:-latest}

services:
  server:
    <<: *rsnova-common
    profiles: [server]
    restart: unless-stopped
    ports:
      - "48100:48100/tcp"
      - "48100:48100/udp"   # QUIC
      - "48102:48102"       # admin
    # network_mode: host  # recommended for tunnel deployments (see Tunnel + Docker section)
    volumes:
      - ./rsnova_data:/data:ro
    command: ["-c", "/data/server.toml"]

  proxy:
    <<: *rsnova-common
    profiles: [client_proxy]
    restart: unless-stopped
    ports:
      - "48101:48101"
      - "48103:48103"       # admin
    # network_mode: host
    volumes:
      - ./rsnova_data:/data:ro
    command: ["-c", "/data/client_proxy.toml"]

  tunnel:
    <<: *rsnova-common
    profiles: [client_tunnel]
    restart: unless-stopped
    # Tunnel client uses host network: Docker DNS unavailable,
    # remote defaults to 127.0.0.1.
    network_mode: host
    volumes:
      - ./rsnova_data:/data:ro
    command: ["-c", "/data/client_tunnel.toml"]
```

## Tunnel + Docker Deployment Modes

NAT traversal tunnel ports are **bound by the server**, not the tunnel client. When a visitor connects to a tunnel port (e.g., 8080), they connect to the server's host. This has implications for Docker networking:

| Server mode | Tunnel port accessibility | When to use |
|---|---|---|
| `network_mode: host` | Tunnel ports directly accessible on host | **Production** — recommended |
| Bridge + explicit `-p 8080:8080` | Only explicitly mapped ports accessible | Few specific tunnel ports |
| Bridge + port range | Extremely slow startup, not recommended | Never |

**Recommended production setup for tunnel:**
```bash
# Server with host networking (tunnel ports directly accessible)
docker compose --profile server up -d  # with network_mode: host uncommented

# Tunnel client (already uses host networking)
docker compose --profile client_tunnel up -d
```

**Bridge mode limitations:** If server runs in bridge mode, each tunnel port the server binds must be individually published in `docker-compose.yml`. The compose file only exposes 48100 (service) and 48102 (admin) by default. Users must add tunnel ports manually:
```yaml
  server:
    ports:
      - "48100:48100/tcp"
      - "48100:48100/udp"
      - "48102:48102"
      - "8080:8080"     # tunnel port
      - "8443:8443"     # tunnel port
```

## `--init-dir` Command

### Invocation

Uses `--init-dir <PATH>` (named argument, not positional) to avoid ambiguity with other flags:

```bash
# Default: output to current directory
rsnova --init-dir .

# Specify output directory
rsnova --init-dir /data

# Docker usage (Phase 1 — non-interactive, no -it needed)
docker run --rm -v ./rsnova_data:/data rsnova --init-dir /data
```

### Phase 1 behavior (non-interactive)

Phase 1 `--init-dir` always runs in non-interactive mode: no role selection, no prompts. A single invocation writes **all** files needed for compose multi-profile testing:

- `cert.pem`, `key.pem` — shared self-signed certificate
- `server.toml`, `client_proxy.toml`, `client_tunnel.toml` — static templates with defaults from this spec

Existing files are skipped unless `--force` is passed. Like `--rcgen`, `--init-dir` exits immediately after writing files (does not start the service).

Phase 2 adds TTY detection, role selection, and certificate reuse prompts (see Interactive Flow below).

### Preserves `--rcgen`

The existing `--rcgen` flag is preserved for backward compatibility. Internally, the `rcgen()` function is refactored to accept an output directory parameter (currently hardcoded to `./`). `--init-dir` reuses this refactored function.

### New CLI flags

| Flag | Type | Description |
|---|---|---|
| `--init-dir` | `Option<PathBuf>` | Trigger init, output to given directory |
| `--force` | `bool` | Used with `--init-dir`: overwrite existing files without prompting |

### Existing File Handling

If `cert.pem`, `key.pem`, or any config file already exists in the output directory:

- **Phase 1 (always non-interactive)**: Skip existing files by default. Use `--force` to overwrite all generated files without prompting.
- **Phase 2 (interactive, TTY)**: For certificates, if `cert.pem` and `key.pem` already exist, prompt early: "Existing certificates found. Reuse them? [Y/n]" — if yes, skip certificate generation entirely. For config files, prompt before overwriting. Non-TTY behavior unchanged (skip unless `--force`).

```bash
# Phase 1 — skip existing files (default)
docker run --rm -v ./rsnova_data:/data rsnova --init-dir /data

# Phase 1 — force overwrite
docker run --rm -v ./rsnova_data:/data rsnova --init-dir /data --force

# Phase 2 — interactive prompts (requires -it)
docker run --rm -it -v ./rsnova_data:/data rsnova --init-dir /data
```

### Interactive Flow (Phase 2)

Phase 1 generates static templates with defaults. Phase 2 adds interactive questionnaire:

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
| 5 | Admin listen address | 0.0.0.0:48103 |

Note: `tls://server:48100` relies on Docker Compose network DNS. When running standalone (`docker run` without compose), use the actual server IP or `host.docker.internal`.

#### Client Tunnel Questions

| # | Question | Default |
|---|---|---|
| 2 | TLS hostname (for cert + runtime SNI) | mydomain.io |
| 3 | Remote server address | tls://127.0.0.1:48100 |
| 4 | Tunnel client ID | my-client |
| 5 | Tunnel port mappings | 8080:80 |
| 6 | Admin listen address | 0.0.0.0:48104 |

Tunnel port mapping formats (shown as hint during interactive init):
- `port` — same local and remote port
- `local:remote` — map local port to remote port
- `host:local:remote` — bind to specific host
- `host:local:remote:sni` — with SNI routing (e.g., `localhost:9991:8443:api.example.com`)

### Output

All files written to the specified directory:

- `cert.pem` — self-signed TLS certificate
- `key.pem` — private key
- **Phase 1**: `server.toml`, `client_proxy.toml`, and `client_tunnel.toml` (all three, static defaults)
- **Phase 2**: role-specific config for the selected role only (interactive customization)

### TLS Hostname Handling

- **Server**: TLS hostname is used only for certificate generation. Not written to `server.toml`.
- **Client (proxy/tunnel)**: TLS hostname is used for certificate generation AND written to config file (needed at runtime for SNI verification).

## Configuration File Format

**Important**: TOML keys must use snake_case (matching Rust struct field names). `clap-serde-derive` does NOT support kebab-case in TOML — verified by testing that kebab-case keys are silently ignored.

### server.toml

```toml
# === Server Configuration ===
# Server listens on TLS (TCP) + QUIC (UDP) simultaneously on the same port.
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
# NOTE: If enabling file logging to /data/, remove :ro from the volume mount.
```

### client_proxy.toml

```toml
# === Client Proxy Configuration ===
role = "client"
remote = "tls://server:48100"
listen = "0.0.0.0:48101"
admin_listen = "0.0.0.0:48103"
cert = "/data/cert.pem"
tls_host = "mydomain.io"

# === Optional (uncomment to customize) ===
# concurrent = 5
# threads = 2
# idle_timeout_secs = 120
# mux_stream_window = 262144
# max_connections = 256
# log = ""
# NOTE: "remote" uses Docker DNS name "server". For standalone docker run,
# replace with actual IP or host.docker.internal.
# NOTE: If enabling file logging to /data/, remove :ro from the volume mount.
```

### client_tunnel.toml

Note: `listen` is intentionally omitted — it conflicts with `tunnel` in the Args struct (`conflicts_with_all = ["listen", "tproxy"]`). Including both would cause a parse error.

Note: `remote` defaults to `127.0.0.1` because tunnel service uses `network_mode: host`, where Docker DNS is unavailable.

```toml
# === Client Tunnel Configuration ===
role = "client"
remote = "tls://127.0.0.1:48100"
admin_listen = "0.0.0.0:48104"
cert = "/data/cert.pem"
tls_host = "mydomain.io"
tunnel_client_id = "my-client"
# Formats: "port" | "local:remote" | "host:local:remote" | "host:local:remote:sni"
tunnel = ["8080:80", "8443:443"]

# === Optional (uncomment to customize) ===
# threads = 2
# idle_timeout_secs = 120
# mux_stream_window = 262144
# log = ""
# NOTE: If enabling file logging to /data/, remove :ro from the volume mount.
```

## Server Protocol Behavior

The server always starts **both TLS and QUIC** listeners on the same `listen` address (TLS on TCP, QUIC on UDP). There is currently no configuration to disable either protocol.

- Docker compose exposes both `48100/tcp` and `48100/udp` for the server service
- Clients select protocol via the `remote` URL scheme: `tls://` for TLS, `quic://` for QUIC
- **Docker bridge mode note**: For QUIC clients to reach the server, the UDP port mapping (`48100:48100/udp`) is required. Default compose includes this.

## Usage Flow

```bash
# 1. Build image
docker compose build

# 2. Initialize (Phase 1: certs + all three role configs, non-interactive)
docker run --rm -v ./rsnova_data:/data rsnova --init-dir /data

# 3. Edit config if needed
vim rsnova_data/server.toml

# 4. Start a role
docker compose --profile server up -d
docker compose --profile client_proxy up -d
docker compose --profile client_tunnel up -d

# 5. Restart after config change (no rebuild needed)
docker compose --profile server restart

# 6. Run multiple roles together (local testing)
# Each role uses a different admin port (48102/48103/48104) — no conflicts.
docker compose --profile server --profile client_proxy up -d

# Phase 1 already generates all three configs in step 2 — no second init needed.
# Phase 2 only: re-run init to customize a single role interactively
# docker run --rm -it -v ./rsnova_data:/data rsnova --init-dir /data
# → "Existing certificates found. Reuse them? [Y/n]" → Y
```

## Implementation Phases

### Phase 1: Static init + Dockerfile + Compose

1. **CI**: Verify existing `x86_64-unknown-linux-musl` release artifact works in Alpine container
2. **Dockerfile**: Implement as specified (amd64 only)
3. **`--init-dir` (non-interactive)**: Refactor `rcgen()` to accept output path; generate `cert.pem`, `key.pem`, and all three static TOML templates (`server.toml`, `client_proxy.toml`, `client_tunnel.toml`); implement `--force` flag; exit before `service_main`
4. **docker-compose.yml**: Implement profiles with distinct admin ports; server publishes TCP+UDP
5. **`.dockerignore`**: Update to reflect current project state
6. **Smoke test**: `scripts/docker_smoke.sh` — build, init, start server + proxy, verify TLS connectivity

### Phase 2: Interactive init + arm64

1. **CI**: Add `aarch64-unknown-linux-musl` target to `build-release.yml`
2. **Dockerfile**: Enable arm64 architecture mapping
3. **`--init-dir` (interactive)**: TTY detection, role-specific questionnaire, certificate reuse prompts
4. **Tunnel format hints**: Provide examples during interactive init for multi-segment tunnel formats
5. **Smoke test**: Extend to cover tunnel (server host network + tunnel client)

### Phase 3: Enhancements (optional)

1. **Hot reload**: Document that tunnel config can be updated via admin API (`AppConfig::trigger_reload()`) without restart; other config changes still require `docker compose restart`
2. **Docker Hub publishing**: CI workflow to push pre-built images
3. **Compose healthcheck**: Add optional healthcheck profile for Swarm/orchestrated deployments

## Design Decisions

1. **Profiles over separate services** — `docker compose up` without profile starts nothing (safe). Explicit activation required.
2. **Phased init** — Phase 1 generates all three static TOML templates plus shared certs in one non-interactive run. Phase 2 adds interactive role selection and questionnaire. Avoids blocking Docker support on the full interactive implementation.
3. **TOML config over env vars / command args** — Config file can be edited and service restarted without rebuilding container. Tunnel config supports admin hot-reload (Phase 3 documentation).
4. **`--init-dir` complements `--rcgen`** — `--init-dir` is the new entry point for initialization (certs + config). `--rcgen` is preserved for backward compatibility. Uses named argument (not positional) to avoid clap parsing ambiguity.
5. **Alpine only** — s2n-quic + musl compatibility verified. No Debian fallback needed.
6. **QUIC included** — `s2n-quic` is already an unconditional dependency, no feature flag changes needed. Server compose publishes both TCP and UDP.
7. **Flat data directory** — All generated files (certs + config) live at the root of the data volume, no subdirectories.
8. **snake_case in TOML** — Verified that `clap-serde-derive` only recognizes snake_case keys. kebab-case is silently ignored.
9. **Distinct admin ports per role** — server=48102, proxy=48103, tunnel=48104. Prevents bind conflicts when running multiple roles on one host.
10. **Tunnel defaults to host networking** — Uses `network_mode: host` because (a) large port ranges perform poorly in bridge mode, (b) Docker DNS is unavailable in host mode so `remote` defaults to `127.0.0.1`. Server should also use host network for production tunnel deployments (tunnel ports bound by server need to be accessible).
11. **No healthcheck in default compose** — Avoids dependency on wget in runtime image and admin port assumptions. Users needing healthcheck for orchestration can add it per their setup.
12. **Deterministic download URL** — Uses fixed URL template (`/releases/download/<tag>/rsnova-<version>-<target>.tar.gz`) rather than API parsing for versioned builds. Only `latest` resolves via API (single `grep` for `tag_name`). Build fails with clear error message if release/artifact is missing.
13. **Read-only volume with log caveat** — Data volume mounted `:ro` by default for security. If file logging is enabled (`log = "..."`), user must either remove `:ro` or set log path outside `/data/`.
