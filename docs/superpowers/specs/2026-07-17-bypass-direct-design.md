# Bypass Direct: Client Proxy IP/Domain Range Routing

**Date:** 2026-07-17
**Status:** Draft

## Overview

Support bypass routing in client proxy mode: when the target IP matches a CIDR range or the target domain matches a domain suffix rule, the connection is forwarded directly to the target (bypassing the remote tunnel server). Non-matching traffic continues through the existing remote tunnel path.

## Motivation

In client proxy mode, all traffic currently goes through the remote tunnel server. For internal/private network destinations (e.g., `10.0.0.0/8`, `*.corp.internal`), routing through the remote server is unnecessary and adds latency. A direct bypass mechanism allows these destinations to be reached locally while other traffic continues through the tunnel.

## Configuration

### Format

Plain text file, one rule per line. Empty lines and `#`-prefixed lines are comments.

```
# Direct bypass rules — CIDR
10.0.0.0/8
192.168.0.0/16

# Direct bypass rules — domain suffix
*.corp.internal
*.local
db.internal.corp.com
```

### CLI Parameter

```
--bypass-file <path>    # Default: bypass.txt (current working directory)
--no-default-bypass     # Disable built-in default rules
```

### Built-in Default Rules

These are always active unless `--no-default-bypass` is specified. Config file rules are appended on top of defaults.

**CIDR:**
```
127.0.0.0/8
10.0.0.0/8
172.16.0.0/12
192.168.0.0/16
169.254.0.0/16
::1/128
fc00::/7
fe80::/10
```

**Domain:**
```
localhost
*.localhost
```

### Hot Reload

A background task polls the bypass file mtime every 5 seconds. On change, rules are re-parsed and atomically swapped via `RwLock`. Parse errors are logged and the previous rules are retained.

## Data Flow

```
handle_local_tunnel(inbound, sender, bypass_cfg)
  |
  +-- Protocol detection (peek 3 bytes)
  |     |
  |     +-- SOCKS5  → handle_socks5()
  |     +-- TLS SNI → handle_tls()
  |     +-- HTTP     → handle_http() / handle_https()
  |     +-- Default  → handle_transparent()
  |
  +-- Each handler:
        |
        +-- Parse target address
        +-- try_bypass_direct(tunnel_id, &mut inbound, &target, &bypass_cfg, idle_timeout)
              |
              +-- Match target against bypass rules
              |     |
              |     +-- IP target    → CIDR range check
              |     +-- Domain target → domain suffix check
              |
              +-- No match → return None → handler continues to remote tunnel
              +-- Match:
                    +-- TcpStream::connect(target) with timeout
                    +-- Success → Stream::transfer(inbound, target_conn) → return Some(Ok(()))
                    +-- Failure → return Some(Err(...))
```

## Matching Logic

- **Target is an IP address** (e.g., `10.0.1.5:8080`): iterate CIDR rules, check containment. No DNS resolution.
- **Target is a domain** (e.g., `api.corp.internal:443`): iterate domain suffix rules. A rule `*.corp.internal` matches `api.corp.internal` and `x.y.corp.internal`. A rule without `*.` prefix (e.g., `db.internal.corp.com`) does exact match only. No DNS resolution for IP rules — domain targets only use domain rules.

## Components

### New: `src/tunnel/bypass.rs`

```
BypassConfig {
    cidrs: RwLock<Vec<IpNet>>,
    domain_suffixes: RwLock<Vec<String>>,
    file_path: PathBuf,
}

BypassConfig::load(file_path, include_defaults) → Arc<BypassConfig>
BypassConfig::reload_from_file(&self) → Result<()>
BypassConfig::is_match(target: &str) → bool

try_bypass_direct(tunnel_id, inbound, target, config, idle_timeout_secs) → Option<Result<()>>
start_bypass_watcher(config: Arc<BypassConfig>)
```

### Modified: `src/tunnel/local.rs`

- `handle_local_tunnel` accepts `bypass_cfg: Arc<BypassConfig>` and `idle_timeout_secs: usize`, passes to each handler
- `start_local_tunnel_server` accepts `bypass_cfg: Arc<BypassConfig>`, passes to `handle_local_tunnel`

### Modified: Protocol Handlers

Each handler (`socks5_local.rs`, `tls_local.rs`, `http_local.rs`, `transparent.rs`) gains one call site after target parsing:

```rust
if let Some(result) = try_bypass_direct(tunnel_id, &mut inbound, &target, &bypass_cfg, idle_timeout_secs).await {
    return result;
}
```

### Modified: `src/main.rs`

- New CLI arg: `--bypass-file` (default: `bypass.txt`)
- New CLI arg: `--no-default-bypass` (flag)
- Load `BypassConfig` in `service_main`, pass to `start_local_tunnel_server`
- Start bypass watcher background task

### New dependency: `ipnet`

Used for CIDR matching (`IpNet::contains`).

## Error Handling

| Scenario | Behavior |
|----------|----------|
| Bypass file does not exist | No custom rules; built-in defaults active. Log info. |
| Malformed line in config | Skip line, log warn. |
| Hot reload parse error | Keep current rules, log warn. |
| Direct TCP connect timeout (10s) | Return `Some(Err(...))` to handler; handler returns error to caller. |
| Direct TCP connect refused | Same as above. |
| Empty config / all rules removed | Built-in defaults still active (unless `--no-default-bypass`). |

## Testing

- Unit tests for `BypassConfig::is_match` (IP, domain, suffix, no match)
- Unit tests for config file parsing (comments, empty lines, mixed rules)
- Unit tests for default rule loading
- Manual integration test: start client proxy with bypass file, verify internal IP goes direct, external goes remote
