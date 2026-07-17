# Client Proxy Direct-Bypass (Split Tunneling)

**Date:** 2026-07-17
**Status:** Approved design, pending implementation plan
**Scope:** `--role client` local-proxy mode (the `start_local_tunnel_server` path). Tunnel mode (`--tunnel`) and server mode are out of scope.

## Goal

In client proxy mode, forward traffic whose destination matches configured rules **directly** from the client (local `TcpStream::connect`) instead of opening a mux stream to the remote server. This bypasses the tunnel for destinations that should not (or cannot) be reached via the remote — e.g. loopback, private/intranet ranges, `localhost`.

## Non-goals

- UDP / tproxy direct bypass (TCP only for this iteration).
- DNS resolution on the client to match domain targets against IP ranges. Domain targets match by name only.
- Server-side or tunnel-mode changes.
- A "force remote" (inverse) rule list. Only a direct-bypass list is supported.

## Decisions (from brainstorming)

1. **Matching model:** Two rule kinds — CIDR rules (match IP-literal destinations) and domain-suffix rules (match domain destinations by name). No client-side DNS resolution. (Choice C.)
2. **Configuration source:** Plain-text rules file, one rule per line, auto-classified. Path supplied via `--direct-rules <path>` (also a valid TOML key `direct_rules`). (Supersedes an earlier two-CLI-flag design.)
3. **Hot reload:** Rules are reloadable at runtime via the existing admin reload path; reload re-reads the file and swaps the compiled rule set. (Choice B.)
4. **Direct-connect failure behavior:** Close the inbound connection; do **not** fall back to the remote server. (Choice A.)
5. **Transport scope:** TCP only. (Choice A.)
6. **Default rules, on by default:** Direct bypass is enabled by default with a built-in default rule set (loopback / private / link-local / `localhost`). `--direct-rules <file>` adds file rules on top of the defaults. `--no-direct-bypass` disables the feature entirely. (Choice A.) Rationale: traffic to `127.0.0.1`/`192.168.x` should not be detoured through the remote server (it would wrongly hit the remote's loopback/private addresses); defaulting these to direct is a correctness improvement.
7. **Implementation shape:** A single shared `try_bypass_direct` method invoked from each of the 4 local handlers — no new `Message` variant, no extra mpsc channel.

## Architecture

### New module `src/tunnel/direct.rs`

Holds all direct-bypass logic behind a small, testable surface.

```rust
/// Compiled, immutable rule snapshot. Replaced as a whole on reload.
pub struct DirectRules {
    cidrs: Vec<IpNet>,            // IPv4/IPv6 CIDRs (incl. /32, /128 from bare IPs)
    domain_suffixes: Vec<String>, // lowercased, leading dots stripped, e.g. "corp.local"
}

impl DirectRules {
    /// Built-in defaults: loopback, private, link-local, ULA + "localhost".
    pub fn defaults() -> Self;
    /// Read + classify each line of the file. Never returns Err for a bad line
    /// (warn-skips). Returns Err only on file IO failure.
    pub fn parse_file(path: &Path) -> Result<Self>;
    /// Merge two rule sets (defaults ⊕ file). Duplicates allowed; cheap to scan.
    pub fn merge(self, other: Self) -> Self;
    /// host is either an IP literal (matched against cidrs) or a domain name
    /// (suffix-matched against domain_suffixes).
    pub fn matches(&self, host: &str) -> bool;
    pub fn is_empty(&self) -> bool;
}

/// Shared handle held by the local handlers.
#[derive(Clone)]
pub struct DirectCtx {
    pub enabled: bool,
    pub path: Option<PathBuf>,                 // Some when --direct-rules given
    pub rules: Arc<RwLock<DirectRules>>,       // std::sync::RwLock; no await under lock
    pub idle_timeout_secs: usize,
}

impl DirectCtx {
    /// Returns Ok(true) if the stream was handled by direct bypass (success or
    /// failure — caller must return Ok(()) either way). Ok(false) = not matched,
    /// caller proceeds to remote.
    pub async fn try_bypass(
        &self, tunnel_id: u32, inbound: TcpStream, target_addr: &str,
        payload: Option<Vec<u8>>,
    ) -> Result<bool>;

    /// Re-read file (if path set), recompute defaults ⊕ file, swap in. On file
    /// error: warn and keep previous rules.
    pub fn reload(&self) -> Result<()>;
}
```

### Rule file format

Plain text, one rule per line:

- Blank lines and lines beginning with `#` are ignored.
- Each remaining line is classified, in order:
  1. Parse as `IpNet` (CIDR, e.g. `10.0.0.0/8`, `fe80::/10`) → CIDR rule.
  2. Else parse as `IpAddr` (bare IP, e.g. `1.2.3.4`) → CIDR rule as `/32` (IPv4) or `/128` (IPv6).
  3. Else treat as a **domain-suffix** rule: strip a leading `.`, lowercase.
- A line that is none of the above is skipped with a `warn!` log. Loading continues; the file is not rejected as a whole.

### Matching semantics

- `matches(host)`:
  - If `host.parse::<IpAddr>()` succeeds → return `cidrs.iter().any(|c| c.contains(&ip))`.
  - Else lowercase `host`; return true if `host == suffix` or `host.ends_with(&format!(".{suffix}"))` for any suffix. The leading-dot guard prevents `notcorp.local` from matching `corp.local`.
- Domain matching is case-insensitive; CIDR matching is exact.

### Built-in default rules

- CIDR: `127.0.0.0/8`, `::1/128`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`, `169.254.0.0/16`, `fe80::/10`.
- Domain: `localhost`.

### New CLI / TOML args (`main.rs` `Args`)

- `--direct-rules <path>` (`direct_rules = "..."`): optional path to the plain-text rules file. File rules are merged on top of the built-in defaults.
- `--no-direct-bypass` (`no_direct_bypass = true`): disables the feature entirely. Default `false` (feature on with defaults).

### Data flow

```
inbound → handle_local_tunnel (peek routing) → handler extracts target_addr
        → ctx.try_bypass(tunnel_id, inbound, target_addr, payload)
            ├─ enabled=false or rules empty → Ok(false)
            ├─ no match                     → Ok(false) → handler sends Message::open_tcp_stream → mux_client_loop → remote
            └─ match:
                 TcpStream::connect(target_addr) [timeout DEFAULT_TIMEOUT_SECS]
                   ├─ fail → warn, close inbound → Ok(true)            (decision 4: no fallback)
                   └─ ok   → write payload (if any) → Stream::transfer(idle_timeout) → Ok(true)
```

### Handler changes

`handle_local_tunnel` (`local.rs`) and `start_local_tunnel_server` gain a `direct_ctx: DirectCtx` parameter, threaded from `service_main`'s client-proxy branch. Each handler calls `ctx.try_bypass(...)` immediately after it has `target_addr`, before sending to the remote `ProxySender`:

1. `socks5_local::handle_socks5` — after writing the SOCKS5 SUCCESS reply, with `payload = None`.
2. `http_local::handle_http` — after `extract_target`, with `payload = Some(headers_buf)`.
3. `http_local::handle_https` — after writing `200 Connection established` and peeking SNI, with `payload = None` (SNI bytes remain buffered in `inbound` and are relayed normally).
4. `transparent::handle_transparent` — after `get_original_dst` (already an IP, ideal for CIDR matching).

On `Ok(true)` the handler returns `Ok(())` immediately; on `Ok(false)` it continues with the existing `Message::open_tcp_stream(...).send(sender)` path.

Note: SOCKS5/HTTPS handlers already send the success/established reply before the remote connect is attempted in the current code. Direct bypass preserves this — a direct-connect failure after a sent success reply simply closes the stream, matching existing behavior when a remote open later fails.

### `try_bypass` internal steps

1. If `!self.enabled` → return `Ok(false)`.
2. Read-lock `rules`; if `rules.is_empty()` → return `Ok(false)` (zero-cost short-circuit for the rare empty case).
3. Extract `host` from `target_addr` (see host extraction). If extraction fails → return `Ok(false)` (conservative: route to remote rather than block).
4. `rules.matches(host)` false → return `Ok(false)`.
5. `tokio::time::timeout(DEFAULT_TIMEOUT_SECS, TcpStream::connect(target_addr)).await`. On error/timeout → `warn!`, drop inbound, return `Ok(true)`.
6. If `payload` is `Some` → `outbound.write_all(payload).await`; on error → debug log, return `Ok(true)`.
7. `Stream::new(&mut inbound_r, &mut inbound_w, &mut outbound_r, &mut outbound_w).transfer(idle_timeout_secs).await` (same construction as `handle_server_stream` in `stream.rs`).
8. Return `Ok(true)`.

### Host extraction

- Try `target_addr.parse::<SocketAddr>()` first (covers `ip:port` and `[ipv6]:port`); take `.ip()` as host.
- Else split on the last `:`; the part before is `host` (covers `domain:port` and bare `domain`).
- A parse failure / no `:` leaves host extraction failing → `Ok(false)` (route to remote).

### Hot reload

- `service_main` constructs the initial `DirectCtx`: `rules = defaults ⊕ parse_file(path)` (or defaults only if no path / file unreadable at startup — see error handling). `enabled = !args.no_direct_bypass`.
- The same `DirectCtx` is shared with the admin reload path. On admin reload, alongside the existing `trigger_reload()`, call `DirectCtx::reload()`:
  - Re-read `path` (if `Some`), recompute `defaults ⊕ file`, `write-lock` and swap.
  - On file IO error → `warn!` and keep the previous rule set (do not clear, do not revert to defaults-only).
- The local accept loop and handlers read the current rules per connection; no restart needed.
- `ReloadableConfig` gains no new fields — the rule source of truth is the file on disk, not in-memory config.

## Error handling

| Stage | Failure | Behavior |
|---|---|---|
| Startup file load | File missing / IO error | `warn!`, use **defaults only**, continue running (do not exit) |
| Startup / reload line parse | A line cannot be classified | `warn!` skip the line, continue loading the rest |
| Reload file load | IO error | `warn!`, **keep previous rules** (no swap, no clearing) |
| Host extraction | Cannot extract host | Treat as no match → route to remote |
| Direct `TcpStream::connect` | Timeout / refused / unreachable | `warn!`, close inbound, `Ok(true)` (no remote fallback) |
| Payload write / relay | IO error | `debug!` (aligned with existing `transfer finish`), close, `Ok(true)` |

## Logging (all prefixed with `[{}]` tunnel_id where applicable)

- `[tunnel_id] Direct bypass hit: {target_addr} (rule={matched_rule})` — info or debug.
- `[tunnel_id] Direct connect failed: {target_addr}: {e}` — warn.
- `direct rules loaded: {N} cidrs, {M} domains from {path:?}` — info.
- `direct rules file not found, using defaults only: {path:?}: {e}` — warn (startup).
- `direct rules reload failed, keeping previous: {e}` — warn.

## Metrics

- `client_proxy_direct_streams` (gauge) — active direct relays; inc/dec aligned with existing `client_proxy_streams`.
- `client_proxy_direct_total` (counter) — direct-bypass hits.
- `client_proxy_direct_connect_failed_total` (counter) — direct-connect failures.
- Unmatched traffic is not counted here (it flows through the existing `client_proxy_streams` path).

## Dependencies

- Add `ipnet` crate (lightweight, pure-Rust, CIDR parse + `contains`). Used for `IpNet` and `IpAddr`-to-/32,/128 promotion.

## Testing (`direct.rs` `#[cfg(test)] mod tests`)

1. **Parse + classify:** a mixed file (CIDR / bare IP / domain / `#` comment / blank / unrecognizable line) yields the right `cidrs` and `domain_suffixes`; bare IP → /32; unrecognizable line skipped without error.
2. **`matches` semantics:**
   - `10.5.0.1` ∈ `10.0.0.0/8` true; `11.0.0.1` false.
   - `::1` ∈ `::1/128`; `fe80::1` ∈ `fe80::/10`.
   - `a.b.corp.local` matches `corp.local`; `corp.local` matches itself; `notcorp.local` does **not** match `corp.local`; case-insensitive.
   - Empty `DirectRules`: `is_empty` true, `matches` always false.
3. **Defaults:** `DirectRules::defaults()` matches `127.0.0.1`, `192.168.1.5`, `localhost`; does not match `8.8.8.8` or `example.com`.
4. **Host extraction:** `127.0.0.1:443`, `[::1]:443`, `example.com:80`, `example.com` all yield the correct host; an unparseable string routes to no-match.
5. **End-to-end relay via `try_bypass`:** with `tokio::io::duplex` standing in for the target, a `DirectCtx` with a hitting rule completes payload write + bidirectional copy + EOF close and returns `Ok(true)`; a non-hitting rule returns `Ok(false)`.

Protocol-level end-to-end (SOCKS5/HTTP/HTTPS parsing) is already covered by the existing handlers; this iteration only adds the `try_bypass` call site, so unit-testing `try_bypass` + `matches` + parse covers the new logic.

## Files touched (summary)

- New: `src/tunnel/direct.rs`.
- `src/tunnel/mod.rs` — declare `direct`.
- `src/main.rs` — new `Args` fields (`direct_rules`, `no_direct_bypass`); construct `DirectCtx` in the client-proxy branch; pass to `start_local_tunnel_server`; wire admin reload to `DirectCtx::reload`.
- `src/tunnel/local.rs` — `handle_local_tunnel` + `start_local_tunnel_server` accept and forward `direct_ctx`.
- `src/tunnel/socks5_local.rs`, `src/tunnel/http_local.rs`, `src/tunnel/transparent.rs` — accept `direct_ctx`, call `try_bypass` after target extraction.
- `src/tunnel/stream.rs` — no change (reuses `Stream::transfer`).
- `src/app_config.rs` / admin — call `DirectCtx::reload` on the reload path (no new `ReloadableConfig` fields).
- `Cargo.toml` — add `ipnet`.
