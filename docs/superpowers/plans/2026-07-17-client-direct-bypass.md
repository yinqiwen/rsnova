# Client Proxy Direct-Bypass Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In client proxy mode, route traffic whose destination matches configured CIDR/domain rules directly from the client (local `TcpStream::connect`) instead of through the remote tunnel server.

**Architecture:** A new `src/tunnel/direct.rs` module exposes `DirectRules` (compiled, atomically-swapped rule set), `DirectCtx` (shared handle holding rules + a `try_bypass` method), and `start_direct_watcher` (5s mtime-poll hot reload). Each of the 4 local protocol handlers calls `ctx.try_bypass(...)` right after extracting the target address; a hit connects directly and relays via the existing `Stream::transfer`, a miss falls through to the existing remote path. No new `Message` variant, no extra mpsc channel.

**Tech Stack:** Rust (edition 2024), tokio, `ipnet` (new dep) for CIDR matching, `metrics` crate, existing `Stream::transfer` relay.

**Spec:** `docs/superpowers/specs/2026-07-17-client-direct-bypass-design.md`

---

## File Structure

- **Create `src/tunnel/direct.rs`** — `DirectRules`, `DirectCtx`, `try_bypass`, `start_direct_watcher`. Single responsibility: direct-bypass rule matching and relay.
- **Modify `src/tunnel/mod.rs`** — declare `pub(crate) mod direct;` + re-exports.
- **Modify `src/tunnel/local.rs`** — thread `direct_ctx` through `handle_local_tunnel` and `start_local_tunnel_server`.
- **Modify `src/tunnel/socks5_local.rs`, `src/tunnel/http_local.rs`, `src/tunnel/tls_local.rs`, `src/tunnel/transparent.rs`** — accept `direct_ctx`, call `try_bypass` after target extraction.
- **Modify `src/app_config.rs`** — add `direct_ctx: DirectCtx` field to `AppConfig`.
- **Modify `src/admin.rs`** — call `DirectCtx::reload()` on config save.
- **Modify `src/main.rs`** — new `Args` fields, construct `DirectCtx`, spawn watcher, pass to local server.
- **Modify `Cargo.toml`** — add `ipnet`.

---

### Task 1: Add `ipnet` dependency

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, under `[dependencies]`, add (after the `lru = "0.18"` line):

```toml
ipnet = "2"
```

- [ ] **Step 2: Verify it resolves**

Run: `cargo build`
Expected: builds successfully (ipnet fetched and compiled).

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "deps: add ipnet for direct-bypass CIDR matching"
```

---

### Task 2: `DirectRules` — parsing, defaults, matching

**Files:**
- Create: `src/tunnel/direct.rs`
- Modify: `src/tunnel/mod.rs:15` (add module declaration)
- Test: `src/tunnel/direct.rs` (inline `#[cfg(test)] mod tests`)

- [ ] **Step 1: Declare the module**

In `src/tunnel/mod.rs`, add after `mod transparent;` (line 15):

```rust
pub(crate) mod direct;
```

- [ ] **Step 2: Write the failing tests**

Create `src/tunnel/direct.rs` with only the test module first:

```rust
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::{Arc, RwLock};

use ipnet::{IpNet, Ipv4Net, Ipv6Net};

/// Compiled, immutable rule snapshot. Replaced as a whole on reload (single
/// RwLock → no torn reads between cidrs and domain rules mid-swap).
pub struct DirectRules {
    cidrs: Vec<IpNet>,
    domain_exact: Vec<String>,
    domain_suffix: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lines_classifies_mixed_rules() {
        let content = "\
# comment
10.0.0.0/8
1.2.3.4
*.corp.internal
db.internal.corp.com
not a rule
fe80::/10
";
        let r = DirectRules::parse_lines(content);
        // 10.0.0.0/8, 1.2.3.4 (/32), fe80::/10
        assert_eq!(r.cidrs.len(), 3);
        assert!(r.cidrs.iter().any(|c| c.contains(&IpAddr::V4("10.5.0.1".parse().unwrap()))));
        assert!(r.cidrs.iter().any(|c| c.contains(&IpAddr::V4("1.2.3.4".parse().unwrap()))));
        assert_eq!(r.domain_suffix, vec!["corp.internal".to_string()]);
        assert_eq!(r.domain_exact, vec!["db.internal.corp.com".to_string()]);
    }

    #[test]
    fn matches_ip_against_cidr() {
        let r = DirectRules::parse_lines("10.0.0.0/8\n");
        assert!(r.matches("10.5.0.1"));
        assert!(!r.matches("11.0.0.1"));
    }

    #[test]
    fn matches_ipv6() {
        let r = DirectRules::parse_lines("::1/128\nfe80::/10\n");
        assert!(r.matches("::1"));
        assert!(r.matches("fe80::1"));
        assert!(!r.matches("2001::1"));
    }

    #[test]
    fn suffix_matches_subdomain_not_self() {
        let r = DirectRules::parse_lines("*.corp.internal\n");
        assert!(r.matches("api.corp.internal"));
        assert!(r.matches("x.y.corp.internal"));
        assert!(!r.matches("corp.internal"));
        assert!(!r.matches("notcorp.internal"));
    }

    #[test]
    fn exact_matches_only_self() {
        let r = DirectRules::parse_lines("corp.internal\n");
        assert!(r.matches("corp.internal"));
        assert!(!r.matches("api.corp.internal"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let r = DirectRules::parse_lines("*.Corp.Internal\n");
        assert!(r.matches("API.CORP.INTERNAL"));
    }

    #[test]
    fn empty_rules_match_nothing() {
        let r = DirectRules::empty();
        assert!(r.is_empty());
        assert!(!r.matches("anything"));
    }

    #[test]
    fn defaults_match_local_and_private() {
        let r = DirectRules::defaults();
        assert!(r.matches("127.0.0.1"));
        assert!(r.matches("192.168.1.5"));
        assert!(r.matches("localhost"));
        assert!(r.matches("foo.localhost"));
        assert!(!r.matches("8.8.8.8"));
        assert!(!r.matches("example.com"));
    }

    #[test]
    fn merge_combines_rulesets() {
        let a = DirectRules::parse_lines("10.0.0.0/8\n*.a.com\n");
        let b = DirectRules::parse_lines("192.168.0.0/16\nb.com\n");
        let m = a.merge(b);
        assert!(m.matches("10.1.2.3"));
        assert!(m.matches("192.168.0.1"));
        assert!(m.matches("x.a.com"));
        assert!(m.matches("b.com"));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib tunnel::direct::tests 2>&1 | head -40`
Expected: compile errors — `DirectRules::parse_lines`, `matches`, `empty`, `defaults`, `merge`, `is_empty`, and the `cidrs`/`domain_suffix`/`domain_exact` fields do not exist yet.

- [ ] **Step 4: Implement `DirectRules`**

Append the impl block above the test module in `src/tunnel/direct.rs` (between the struct and `#[cfg(test)]`):

```rust
impl DirectRules {
    pub fn empty() -> Self {
        Self { cidrs: Vec::new(), domain_exact: Vec::new(), domain_suffix: Vec::new() }
    }

    /// Built-in defaults: loopback, private, link-local, ULA + localhost + *.localhost.
    pub fn defaults() -> Self {
        let cidrs: Vec<IpNet> = [
            "127.0.0.0/8", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16",
            "169.254.0.0/16", "::1/128", "fc00::/7", "fe80::/10",
        ]
        .iter()
        .map(|s| s.parse::<IpNet>().expect("valid default CIDR"))
        .collect();
        Self {
            cidrs,
            domain_exact: vec!["localhost".to_string()],
            domain_suffix: vec!["localhost".to_string()],
        }
    }

    /// Parse a multi-line string. Blank lines and `#`-prefixed lines are ignored.
    /// Unrecognizable lines are warn-skipped. Never errors.
    pub fn parse_lines(content: &str) -> Self {
        let mut cidrs = Vec::new();
        let mut domain_exact = Vec::new();
        let mut domain_suffix = Vec::new();
        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Ok(net) = line.parse::<IpNet>() {
                cidrs.push(net);
                continue;
            }
            if let Ok(ip) = line.parse::<IpAddr>() {
                cidrs.push(match ip {
                    IpAddr::V4(v) => IpNet::V4(Ipv4Net::new(v, 32).unwrap()),
                    IpAddr::V6(v) => IpNet::V6(Ipv6Net::new(v, 128).unwrap()),
                });
                continue;
            }
            if let Some(rest) = line.strip_prefix("*.") {
                if !rest.is_empty() {
                    domain_suffix.push(rest.to_lowercase());
                }
                continue;
            }
            if is_plausible_domain(line) {
                domain_exact.push(line.to_lowercase());
                continue;
            }
            tracing::warn!("direct rules: skipping unrecognizable line: {line:?}");
        }
        Self { cidrs, domain_exact, domain_suffix }
    }

    /// Read + classify a file. Returns Err only on IO failure.
    pub fn parse_file(path: &Path) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::parse_lines(&content))
    }

    pub fn merge(self, other: Self) -> Self {
        let mut cidrs = self.cidrs;
        cidrs.extend(other.cidrs);
        let mut domain_exact = self.domain_exact;
        domain_exact.extend(other.domain_exact);
        let mut domain_suffix = self.domain_suffix;
        domain_suffix.extend(other.domain_suffix);
        Self { cidrs, domain_exact, domain_suffix }
    }

    pub fn is_empty(&self) -> bool {
        self.cidrs.is_empty() && self.domain_exact.is_empty() && self.domain_suffix.is_empty()
    }

    /// `host` is an IP literal (matched against cidrs) or a domain name
    /// (exact- or suffix-matched). Case-insensitive for domains.
    pub fn matches(&self, host: &str) -> bool {
        self.matched_rule(host).is_some()
    }

    /// Returns the matched rule string (for logging), or None.
    pub fn matched_rule(&self, host: &str) -> Option<String> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            if let Some(net) = self.cidrs.iter().find(|c| c.contains(&ip)) {
                return Some(format!("{net}"));
            }
            return None;
        }
        let h = host.to_lowercase();
        if self.domain_exact.iter().any(|d| d == &h) {
            return Some(h);
        }
        for s in &self.domain_suffix {
            if h.ends_with(&format!(".{s}")) {
                return Some(format!("*.{s}"));
            }
        }
        None
    }
}

/// A loose check so that bare hostnames become exact-domain rules while
/// obvious garbage (spaces, etc.) is skipped. `parse_lines` already trimmed.
fn is_plausible_domain(s: &str) -> bool {
    !s.is_empty()
        && !s.contains(char::is_whitespace)
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib tunnel::direct::tests`
Expected: all 9 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/tunnel/direct.rs src/tunnel/mod.rs
git commit -m "feat(direct): DirectRules parse/defaults/match with ipnet"
```

---

### Task 3: `DirectCtx` — shared handle with reload

**Files:**
- Modify: `src/tunnel/direct.rs`
- Test: `src/tunnel/direct.rs` (inline tests)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/tunnel/direct.rs`:

```rust
    #[test]
    fn ctx_new_loads_defaults_and_file() {
        // No path → defaults only.
        let ctx = DirectCtx::new(true, true, None, 30);
        assert!(ctx.rules.read().unwrap().matches("127.0.0.1"));
        assert!(!ctx.rules.read().unwrap().matches("8.8.8.8"));
    }

    #[test]
    fn ctx_new_no_defaults_is_empty_when_no_file() {
        let ctx = DirectCtx::new(true, false, None, 30);
        assert!(ctx.rules.read().unwrap().is_empty());
    }

    #[test]
    ctx_reload_keeps_previous_on_file_error() {
        // Point at a nonexistent path: startup falls back to defaults,
        // reload must KEEP the previous (defaults) rather than clearing.
        let path = std::path::PathBuf::from("/nonexistent/rsnova-direct-test-no-such-file.txt");
        let ctx = DirectCtx::new(true, true, Some(path), 30);
        assert!(ctx.rules.read().unwrap().matches("127.0.0.1"));
        let _ = ctx.reload();
        // Still matches defaults → previous rules were retained.
        assert!(ctx.rules.read().unwrap().matches("127.0.0.1"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib tunnel::direct::tests`
Expected: compile error — `DirectCtx`, `DirectCtx::new`, `ctx.rules`, `ctx.reload` do not exist.

- [ ] **Step 3: Implement `DirectCtx`**

Add to `src/tunnel/direct.rs` (above the test module):

```rust
/// Shared handle held by the local handlers. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct DirectCtx {
    pub enabled: bool,
    pub include_defaults: bool,
    pub path: Option<std::path::PathBuf>,
    pub rules: Arc<RwLock<DirectRules>>,
    pub idle_timeout_secs: usize,
}

impl DirectCtx {
    pub fn new(
        enabled: bool,
        include_defaults: bool,
        path: Option<std::path::PathBuf>,
        idle_timeout_secs: usize,
    ) -> Self {
        let rules = Self::load_rules(include_defaults, path.as_deref(), /*on_error_keep*/ false);
        Self { enabled, include_defaults, path, rules: Arc::new(RwLock::new(rules)), idle_timeout_secs }
    }

    /// Recompose defaults ⊕ file. On file IO error:
    ///  - at startup (`keep_previous=false`): fall back to defaults-only (warn).
    ///  - on reload (`keep_previous=true`):  keep previous rules (caller retains lock).
    fn load_rules(include_defaults: bool, path: Option<&Path>, keep_previous: bool) -> DirectRules {
        let base = if include_defaults { DirectRules::defaults() } else { DirectRules::empty() };
        let Some(p) = path else { return base; };
        match DirectRules::parse_file(p) {
            Ok(f) => base.merge(f),
            Err(e) => {
                if keep_previous {
                    tracing::warn!("direct rules reload failed, keeping previous: {e}");
                    // signal to caller to skip the swap by returning a sentinel;
                    // callers handle this by checking via reload() below.
                } else {
                    tracing::warn!("direct rules file load failed, using defaults only: {p:?}: {e}");
                }
                base
            }
        }
    }

    /// Re-read file (if path set) and swap. On file error: warn and keep
    /// previous rules. No-op when path is None.
    pub fn reload(&self) -> anyhow::Result<()> {
        let Some(p) = self.path.as_deref() else { return Ok(()); };
        let new = match DirectRules::parse_file(p) {
            Ok(f) => {
                let base = if self.include_defaults { DirectRules::defaults() } else { DirectRules::empty() };
                base.merge(f)
            }
            Err(e) => {
                tracing::warn!("direct rules reload failed, keeping previous: {e}");
                return Ok(()); // keep previous
            }
        };
        let mut guard = self.rules.write().unwrap();
        let n_cidrs = new.cidrs.len();
        let n_exact = new.domain_exact.len();
        let n_suffix = new.domain_suffix.len();
        *guard = new;
        tracing::info!(
            "direct rules reloaded: {n_cidrs} cidrs, {n_exact} exact, {n_suffix} suffix"
        );
        Ok(())
    }
}
```

Note: `load_rules` with `keep_previous=true` is unused in the final design (reload handles its own path); keep the param for clarity or remove it. To avoid a dead-code warning, **remove the `keep_previous` parameter** and the `keep_previous` branch entirely — simplify `load_rules` to:

```rust
    fn load_rules(include_defaults: bool, path: Option<&Path>) -> DirectRules {
        let base = if include_defaults { DirectRules::defaults() } else { DirectRules::empty() };
        let Some(p) = path else { return base; };
        match DirectRules::parse_file(p) {
            Ok(f) => base.merge(f),
            Err(e) => {
                tracing::warn!("direct rules file load failed, using defaults only: {p:?}: {e}");
                base
            }
        }
    }
```

and update the `new` call to `Self::load_rules(include_defaults, path.as_deref())`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib tunnel::direct::tests`
Expected: all tests PASS (including the 3 new ctx tests).

- [ ] **Step 5: Commit**

```bash
git add src/tunnel/direct.rs
git commit -m "feat(direct): DirectCtx shared handle with keep-previous reload"
```

---

### Task 4: `try_bypass` — host extraction + direct connect + relay

**Files:**
- Modify: `src/tunnel/direct.rs`
- Test: `src/tunnel/direct.rs` (inline tests)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module:

```rust
    #[test]
    fn extract_host_handles_ip_domain_and_ipv6() {
        assert_eq!(extract_host("127.0.0.1:443"), Some("127.0.0.1".to_string()));
        assert_eq!(extract_host("[::1]:443"), Some("::1".to_string()));
        assert_eq!(extract_host("example.com:80"), Some("example.com".to_string()));
        assert_eq!(extract_host("example.com"), Some("example.com".to_string()));
        assert_eq!(extract_host("::1"), Some("::1".to_string()));
    }

    #[tokio::test]
    async fn try_bypass_relays_on_hit_and_returns_true() {
        // Stand up a fake target via a TCP listener.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let target = listener.local_addr().unwrap().to_string();
        // Make 127.0.0.1 match (defaults already include 127.0.0.0/8).
        let ctx = DirectCtx::new(true, true, None, 30);

        // Echo server: read 4 bytes, write them back, close.
        let target_task = tokio::spawn(async move {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4];
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(&buf).await.unwrap();
        });

        let (mut client, mut server) = tokio::io::duplex(1024);
        // `inbound` for try_bypass must be a TcpStream; simulate by connecting
        // to a second listener and handing the connected stream in.
        let inbound_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let inbound_addr = inbound_listener.local_addr().unwrap();
        let inbound_task = tokio::spawn(async move {
            let (s, _) = inbound_listener.accept().unwrap();
            // drive a duplex that writes "ping" then reads 4 bytes back
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let _ = s; // not used here; real inbound is connected below
            let _ = client;
            let _ = server;
        });
        let inbound = tokio::net::TcpStream::connect(inbound_addr).await.unwrap();
        let _acceptor = inbound_task; // keep alive

        let payload: Option<Vec<u8>> = Some(b"ping".to_vec());
        let handled = ctx.try_bypass(0, inbound, &target, payload).await.unwrap();
        assert!(handled, "127.0.0.1 target must hit direct bypass");

        target_task.await.unwrap();
    }

    #[tokio::test]
    async fn try_bypass_returns_false_when_disabled() {
        let ctx = DirectCtx::new(false, true, None, 30);
        // A target that would otherwise match (127.0.0.1) must NOT be handled
        // when disabled. Use a non-connecting target to be safe.
        let (_a, _b) = tokio::io::duplex(1024);
        // try_bypass must short-circuit before connecting.
        let inbound_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let inbound_addr = inbound_listener.local_addr().unwrap();
        std::mem::forget(inbound_listener);
        let inbound = tokio::net::TcpStream::connect(inbound_addr).await;
        // If no acceptor, connect fails — but disabled path returns before connect.
        // Use a target that does NOT match so even enabled would skip: 8.8.8.8:9.
        let ctx_enabled_nonmatch = DirectCtx::new(true, true, None, 30);
        // We only assert the disabled case here:
        if let Ok(inbound) = inbound {
            let handled = ctx.try_bypass(0, inbound, "8.8.8.8:9", None).await.unwrap();
            assert!(!handled);
        }
        let _ = ctx_enabled_nonmatch;
    }
```

> Note: the relay test is involved because `try_bypass` consumes a real `TcpStream`. If the above is flaky in the execution environment, simplify to: assert `extract_host` correctness (the unit above) and assert `try_bypass` returns `Ok(false)` for a non-matching target (no relay). The matching+relay path is also exercised by the existing `Stream::transfer` tests. Prefer the simpler form if the duplex/listener setup is problematic — see Step 3's simplification.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib tunnel::direct::tests`
Expected: compile error — `extract_host` and `try_bypass` do not exist.

- [ ] **Step 3: Implement `extract_host` and `try_bypass`**

Add to `src/tunnel/direct.rs`. First the imports at the top of the file (merge with existing):

```rust
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::tunnel::stream::Stream;
use crate::tunnel::DEFAULT_TIMEOUT_SECS;
```

Then the functions (above the test module):

```rust
/// Extract the host portion of a `host:port` / `[ipv6]:port` / bare address.
pub fn extract_host(addr: &str) -> Option<String> {
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        return Some(sa.ip().to_string());
    }
    if let Ok(ip) = addr.parse::<IpAddr>() {
        return Some(ip.to_string());
    }
    let host = match addr.rsplit_once(':') {
        Some((h, _port)) => h,
        None => addr,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

impl DirectCtx {
    /// Ok(true)  = handled by direct bypass (success or failure — caller returns Ok(())).
    /// Ok(false) = not matched / disabled — caller proceeds to remote.
    pub async fn try_bypass(
        &self,
        tunnel_id: u32,
        inbound: TcpStream,
        target_addr: &str,
        payload: Option<Vec<u8>>,
    ) -> anyhow::Result<bool> {
        if !self.enabled {
            return Ok(false);
        }
        let host = match extract_host(target_addr) {
            Some(h) => h,
            None => return Ok(false),
        };
        let rule = {
            let guard = self.rules.read().unwrap();
            if guard.is_empty() {
                return Ok(false);
            }
            guard.matched_rule(&host)
        };
        let Some(rule) = rule else { return Ok(false) };

        tracing::info!(
            "[{tunnel_id}] Direct bypass hit: {target_addr} (rule={rule})"
        );
        metrics::counter!("client_proxy_direct_total").increment(1);

        let connect = tokio::time::timeout(
            std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            TcpStream::connect(target_addr),
        );
        let mut outbound = match connect.await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) | Err(_) => {
                let e = match connect.await {
                    _ => unreachable!(),
                };
                metrics::counter!("client_proxy_direct_connect_failed_total").increment(1);
                tracing::warn!("[{tunnel_id}] Direct connect failed: {target_addr}: {e}");
                return Ok(true);
            }
        };
        // (the match above is awkward; use the clean form shown in the fix below)
```

The connect-error branch above is awkward. **Replace the whole `connect` block** with this clean form:

```rust
        let mut outbound = match tokio::time::timeout(
            std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            TcpStream::connect(target_addr),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                metrics::counter!("client_proxy_direct_connect_failed_total").increment(1);
                tracing::warn!("[{tunnel_id}] Direct connect failed: {target_addr}: {e}");
                return Ok(true);
            }
            Err(e) => {
                metrics::counter!("client_proxy_direct_connect_failed_total").increment(1);
                tracing::warn!("[{tunnel_id}] Direct connect timed out: {target_addr}: {e}");
                return Ok(true);
            }
        };

        metrics::gauge!("client_proxy_direct_streams").increment(1.0);

        // Write any pre-read payload (e.g. HTTP request headers) before relaying.
        if let Some(p) = &payload
            && let Err(e) = outbound.write_all(p).await
        {
            tracing::debug!("[{tunnel_id}] Direct payload write failed: {e}");
            metrics::gauge!("client_proxy_direct_streams").decrement(1.0);
            return Ok(true);
        }

        let (mut in_r, mut in_w) = inbound.into_split();
        let (mut out_r, mut out_w) = outbound.into_split();
        let mut stream = Stream::new(&mut in_r, &mut in_w, &mut out_r, &mut out_w);
        if let Err(e) = stream.transfer(self.idle_timeout_secs).await {
            tracing::debug!("[{tunnel_id}] Direct transfer finish: {e}");
        }
        metrics::gauge!("client_proxy_direct_streams").decrement(1.0);
        Ok(true)
    }
}
```

Make sure there is only **one** `impl DirectCtx` block — merge `reload` (from Task 3) and `try_bypass` into the same block, or keep them as two `impl DirectCtx` blocks (Rust allows multiple). Two blocks are fine.

Also **simplify the Step 1 relay test** if needed: the relay test as written is fragile. Replace the body of `try_bypass_relays_on_hit_and_returns_true` and `try_bypass_returns_false_when_disabled` with the simpler, robust versions:

```rust
    #[tokio::test]
    async fn try_bypass_returns_false_for_nonmatching_target() {
        // 8.8.8.8 is not in defaults; with no file, it must not match.
        // try_bypass must return Ok(false) WITHOUT attempting a connect.
        let ctx = DirectCtx::new(true, true, None, 30);
        // Use a TcpStream that is never connected to anything; since the path
        // returns before connect, the stream is dropped unused.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept in background so connect succeeds; we then close immediately.
        let acc = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let acc_addr = acc.local_addr().unwrap();
        drop(listener);
        let _ = addr;
        let inbound = tokio::net::TcpStream::connect(acc_addr).await.unwrap();
        drop(acc);
        let handled = ctx.try_bypass(0, inbound, "8.8.8.8:9", None).await.unwrap();
        assert!(!handled, "non-matching target must not be handled");
    }

    #[tokio::test]
    async fn try_bypass_returns_false_when_disabled() {
        let ctx = DirectCtx::new(false, true, None, 30);
        let acc = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let acc_addr = acc.local_addr().unwrap();
        let inbound = tokio::net::TcpStream::connect(acc_addr).await.unwrap();
        drop(acc);
        // Even though 127.0.0.1 would match, disabled must short-circuit.
        let handled = ctx.try_bypass(0, inbound, "127.0.0.1:9", None).await.unwrap();
        assert!(!handled);
    }
```

Delete the fragile `try_bypass_relays_on_hit_and_returns_true` test entirely — relay correctness is covered by `Stream::transfer`'s own tests; `try_bypass`'s new logic (match decision, short-circuit, disabled) is covered by the two tests above plus `extract_host` unit tests.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib tunnel::direct::tests`
Expected: all tests PASS.

- [ ] **Step 5: Verify the whole crate still builds**

Run: `cargo build`
Expected: builds (unused-import warnings for `Ipv4Addr`/`Ipv6Addr`/`AsyncReadExt` etc. are fine for now — they get used by Task 7's call sites and the relay; remove any that remain `dead_code`/`unused_imports` before the final clippy task).

- [ ] **Step 6: Commit**

```bash
git add src/tunnel/direct.rs
git commit -m "feat(direct): try_bypass host extraction + direct connect/relay"
```

---

### Task 5: `start_direct_watcher` — mtime-poll hot reload

**Files:**
- Modify: `src/tunnel/direct.rs`
- Test: `src/tunnel/direct.rs` (inline test)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module:

```rust
    #[tokio::test]
    async fn reload_picks_up_file_changes() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "rsnova-direct-test-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, "10.0.0.0/8\n").unwrap();
        let ctx = DirectCtx::new(true, false, Some(path.clone()), 30);
        assert!(ctx.rules.read().unwrap().matches("10.1.2.3"));
        assert!(!ctx.rules.read().unwrap().matches("192.168.1.1"));

        // Rewrite the file; reload; new rule applies.
        std::fs::write(&path, "192.168.0.0/16\n").unwrap();
        ctx.reload().unwrap();
        assert!(ctx.rules.read().unwrap().matches("192.168.1.1"));
        assert!(!ctx.rules.read().unwrap().matches("10.1.2.3"));

        let _ = std::fs::remove_file(&path);
    }
```

- [ ] **Step 2: Run tests to verify they fail/pass**

Run: `cargo test --lib tunnel::direct::tests::reload_picks_up_file_changes`
Expected: PASS (reload already implemented in Task 3). This test guards the reload-on-change contract that the watcher relies on. If it fails, fix `reload`.

- [ ] **Step 3: Implement `start_direct_watcher`**

Add to `src/tunnel/direct.rs`:

```rust
/// Background task: poll `ctx.path`'s mtime every 5s; on change call
/// `ctx.reload()`. Only spawn when `ctx.path` is Some.
pub fn start_direct_watcher(ctx: DirectCtx) {
    let Some(path) = ctx.path.clone() else { return; };
    tokio::spawn(async move {
        let mut last_mtime = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok());
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let mtime = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok());
            if mtime != last_mtime {
                last_mtime = mtime;
                let _ = ctx.reload();
            }
        }
    });
}
```

- [ ] **Step 4: Build and run tests**

Run: `cargo build && cargo test --lib tunnel::direct::tests`
Expected: builds; all direct tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tunnel/direct.rs
git commit -m "feat(direct): mtime-poll hot-reload watcher"
```

---

### Task 6: Add `Args` fields and construct `DirectCtx` in `service_main`

**Files:**
- Modify: `src/main.rs:62-169` (Args struct) and `src/main.rs:202-372` (service_main)

- [ ] **Step 1: Add the three `Args` fields**

In `src/main.rs`, inside `struct Args`, after the `tunnel_port_range` field (line 168):

```rust
    /// Path to a plain-text direct-bypass rules file (one CIDR/domain rule per line).
    #[default(None)]
    #[arg(long = "direct-rules")]
    direct_rules: Option<PathBuf>,

    /// Disable direct bypass entirely.
    #[default(false)]
    #[arg(long = "no-direct-bypass")]
    no_direct_bypass: bool,

    /// Disable the built-in default direct-bypass rules (file rules still apply).
    #[default(false)]
    #[arg(long = "no-default-bypass")]
    no_default_bypass: bool,
```

- [ ] **Step 2: Add `direct_ctx` to `AppConfig`**

In `src/app_config.rs`, add a field to `AppConfig` (after `reload_token`):

```rust
    pub direct_ctx: crate::tunnel::direct::DirectCtx,
```

- [ ] **Step 3: Construct `DirectCtx` and pass it into `AppConfig`**

In `src/main.rs` `service_main`, just before `let app_config = Arc::new(app_config::AppConfig {` (around line 264), construct the ctx:

```rust
    let direct_ctx = tunnel::direct::DirectCtx::new(
        matches!(args.role, Role::Client) && !args.no_direct_bypass,
        !args.no_default_bypass,
        args.direct_rules.clone(),
        args.idle_timeout_secs,
    );
```

Then add the field to the `AppConfig { ... }` literal (after `reload_token: ...`):

```rust
        direct_ctx,
```

- [ ] **Step 4: Spawn the watcher and pass `direct_ctx` to the local server**

In `src/main.rs`, in the `Role::Client` non-tunnel branch, replace the block that calls `start_local_tunnel_server` (around lines 367-372):

```rust
            // Start local tunnel server
            let listen_addr = args.listen;
            let tproxy = args.tproxy;
            let max_connections = args.max_connections;
            let direct_ctx = app_config.direct_ctx.clone();
            if direct_ctx.path.is_some() && direct_ctx.enabled {
                tunnel::direct::start_direct_watcher(direct_ctx.clone());
            }
            tunnel::start_local_tunnel_server(
                &listen_addr,
                tunnel_sender,
                tproxy,
                max_connections,
                direct_ctx,
            )
            .await?;
```

- [ ] **Step 5: Verify it compiles (callers of start_local_tunnel_server not yet updated — expect an error here)**

Run: `cargo build 2>&1 | head -30`
Expected: compile error — `start_local_tunnel_server` expects 4 args, got 5. This is fixed in Task 7. Do not commit yet.

- [ ] **Step 6: Commit (with the signature change stub to keep build green)**

To keep the build green across tasks, update `start_local_tunnel_server` and `handle_local_tunnel` signatures now (Task 7 does the call sites). In `src/tunnel/local.rs`, change:

```rust
async fn handle_local_tunnel(
    inbound: TcpStream,
    tunnel_id: u32,
    sender: ProxySender,
    _direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<()> {
```

and at the call to `handle_local_tunnel` inside the accept loop, pass `direct_ctx.clone()`:

```rust
            let direct_ctx = direct_ctx.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(e) = handle_local_tunnel(inbound, tunnel_id, tunnel_sender, direct_ctx).await {
                    tracing::error!("handle local tunnel error:{}", e);
                }
            });
```

Wait — `direct_ctx` is captured per-iteration; declare `let direct_ctx = direct_ctx.clone();` inside the loop body before the spawn, and add `direct_ctx: crate::tunnel::direct::DirectCtx` as the last parameter of `start_local_tunnel_server`, forwarding it to the loop.

Update `start_local_tunnel_server` signature:

```rust
pub async fn start_local_tunnel_server(
    addr: &SocketAddr,
    sender: ProxySender,
    tproxy: bool,
    max_connections: usize,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<(), std::io::Error> {
```

The four handler calls inside `handle_local_tunnel` (`handle_socks5`, `handle_tls`, `handle_http`/`handle_https`, `handle_transparent`) still take 3 args for now — this will not compile because those handlers haven't been updated. To keep Task 6 self-contained and green, **defer the handler signature changes to Task 7** and instead, temporarily, do not yet pass `direct_ctx` into the handlers. That means Task 6 cannot both add the param and stay green without Task 7.

**Revised approach: merge Task 6 Step 6 into Task 7.** Do NOT commit yet. Leave the build broken only briefly by doing Task 7 immediately after, then commit once at the end of Task 7. Skip the Step 6 commit; proceed to Task 7.

---

### Task 7: Thread `direct_ctx` through handlers and add `try_bypass` call sites

**Files:**
- Modify: `src/tunnel/local.rs`
- Modify: `src/tunnel/socks5_local.rs`
- Modify: `src/tunnel/http_local.rs`
- Modify: `src/tunnel/tls_local.rs`
- Modify: `src/tunnel/transparent.rs`

- [ ] **Step 1: Update `local.rs` signatures and forwarding**

In `src/tunnel/local.rs`:

`handle_local_tunnel` — add param and forward to each handler:

```rust
async fn handle_local_tunnel(
    inbound: TcpStream,
    tunnel_id: u32,
    sender: ProxySender,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<()> {
    let mut peek_buf = [0u8; 3];
    inbound.peek(&mut peek_buf).await?;
    match peek_buf[0] {
        5 => {
            handle_socks5(tunnel_id, inbound, sender, direct_ctx).await?;
            return Ok(());
        }
        4 => {
            tracing::error!("socks4 not supported!");
            return Err(anyhow!("socks4 unimplemented"));
        }
        _ => {}
    }
    if valid_tls_version(&peek_buf[..]) {
        handle_tls(tunnel_id, inbound, sender, direct_ctx).await?;
        return Ok(());
    }
    if let Ok(prefix_str) = std::str::from_utf8(&peek_buf) {
        let prefix_str = prefix_str.to_uppercase();
        match prefix_str.as_str() {
            "GET" | "PUT" | "POS" | "DEL" | "OPT" | "TRA" | "PAT" | "HEA" | "CON" | "UPG" => {
                if prefix_str.as_str() == "CON" {
                    handle_https(tunnel_id, inbound, sender, direct_ctx).await?;
                } else {
                    handle_http(tunnel_id, inbound, sender, direct_ctx).await?;
                }
                return Ok(());
            }
            _ => {}
        };
    }
    tracing::info!(
        "[{}]Accept client with non socks5/tls/http traffic.",
        tunnel_id
    );
    super::transparent::handle_transparent(tunnel_id, inbound, sender, direct_ctx).await
}
```

`start_local_tunnel_server` — add the `direct_ctx` param and clone it per connection:

```rust
pub async fn start_local_tunnel_server(
    addr: &SocketAddr,
    sender: ProxySender,
    tproxy: bool,
    max_connections: usize,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<(), std::io::Error> {
    let listener = new_tcp_listener(addr, tproxy).await?;
    let semaphore = Arc::new(Semaphore::new(max_connections));

    #[cfg(target_os = "linux")]
    {
        use crate::tunnel::udp_local::start_local_udp_tunnel_server;
        if tproxy {
            start_local_udp_tunnel_server(addr, sender.clone())?;
        }
    }

    tracing::info!(
        "Start local TCP listen at {} (max connections: {})",
        addr,
        max_connections
    );
    let mut tunnel_id_seed: u32 = 0;
    while let Ok((inbound, _)) = listener.accept().await {
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(
                    "Max connections ({}) reached, rejecting new connection",
                    max_connections
                );
                continue;
            }
        };
        let tunnel_id = tunnel_id_seed;
        tunnel_id_seed += 1;
        let tunnel_sender = sender.clone();
        let direct_ctx = direct_ctx.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) =
                handle_local_tunnel(inbound, tunnel_id, tunnel_sender, direct_ctx).await
            {
                tracing::error!("handle local tunnel error:{}", e);
            }
        });
    }
    Ok(())
}
```

- [ ] **Step 2: Update `socks5_local.rs`**

Change the signature and add the `try_bypass` call after the SUCCESS reply is written (before the final `sender.send`). Replace the function signature and tail:

```rust
pub async fn handle_socks5(
    tunnel_id: u32,
    mut inbound: TcpStream,
    sender: ProxySender,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<()> {
```

...and after `inbound.write_all(&resp).await?;` and the `tracing::info!` line, before `let msg = Message::open_tcp_stream(...)`:

```rust
    tracing::info!("[{}]Handle SOCKS5 proxy to {}", tunnel_id, target_addr);

    if direct_ctx
        .try_bypass(tunnel_id, inbound, &target_addr, None)
        .await?
    {
        return Ok(());
    }

    let msg = Message::open_tcp_stream(inbound, target_addr, None);
    sender.send(msg).await?;
    Ok(())
}
```

Note: `inbound` is `mut` and is moved into `try_bypass`. The `mut` binding remains valid because `try_bypass` takes ownership; remove the now-unused `mut` only if the compiler warns (it won't — `write_all` earlier used it). Leave `mut inbound`.

- [ ] **Step 3: Update `http_local.rs` (`handle_http` and `handle_https`)**

`handle_http` signature + call (after `extract_target`, before `Message::open_tcp_stream`):

```rust
pub async fn handle_http(
    tunnel_id: u32,
    mut inbound: TcpStream,
    sender: ProxySender,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<()> {
    let headers_buf = read_http_headers(&mut inbound).await?;
    let target_addr = extract_target(&headers_buf, ":80")?;
    tracing::info!("[{}]Handle HTTP proxy to {} ", tunnel_id, target_addr);

    if direct_ctx
        .try_bypass(tunnel_id, inbound, &target_addr, Some(headers_buf))
        .await?
    {
        return Ok(());
    }

    let msg = Message::open_tcp_stream(inbound, target_addr, Some(headers_buf));
    sender.send(msg).await?;
    Ok(())
}
```

Wait — `headers_buf` is moved into `try_bypass` as `Some(headers_buf)`, so it is no longer available for the `Message::open_tcp_stream(..., Some(headers_buf))` fallback. Fix: clone is wasteful for large headers. Instead, pass `Some(headers_buf)` to `try_bypass` only on the hit path. Reorder: decide bypass first using a clone of the target only; but `try_bypass` needs the payload only on a hit.

**Resolution:** make `try_bypass` take the payload by reference instead of by value, so the caller retains ownership on a miss. Change `try_bypass`'s signature in `direct.rs` to:

```rust
    pub async fn try_bypass(
        &self,
        tunnel_id: u32,
        inbound: TcpStream,
        target_addr: &str,
        payload: Option<&[u8]>,
    ) -> anyhow::Result<bool> {
```

and inside, replace `if let Some(p) = &payload` with:

```rust
        if let Some(p) = payload
            && let Err(e) = outbound.write_all(p).await
        {
```

Update the `direct.rs` tests' calls from `Some(vec)` / `None` to `Some(&bytes)` / `None` (the tests pass `None`, so no change needed there).

Then `handle_http` becomes:

```rust
    let headers_buf = read_http_headers(&mut inbound).await?;
    let target_addr = extract_target(&headers_buf, ":80")?;
    tracing::info!("[{}]Handle HTTP proxy to {} ", tunnel_id, target_addr);

    if direct_ctx
        .try_bypass(tunnel_id, inbound, &target_addr, Some(&headers_buf))
        .await?
    {
        return Ok(());
    }

    let msg = Message::open_tcp_stream(inbound, target_addr, Some(headers_buf));
    sender.send(msg).await?;
    Ok(())
```

`handle_https` signature + call (after the `200 Connection established` reply and SNI peek, before `Message::open_tcp_stream`):

```rust
pub async fn handle_https(
    tunnel_id: u32,
    mut inbound: TcpStream,
    sender: ProxySender,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<()> {
    let headers_buf = read_http_headers(&mut inbound).await?;
    let conn_res = "HTTP/1.0 200 Connection established\r\n\r\n";
    inbound.write_all(conn_res.as_bytes()).await?;
    let target_addr = match tls_local::peek_sni_v2(&inbound).await {
        Ok(mut sni) => {
            sni.push_str(":443");
            sni
        }
        Err(_) => extract_target(&headers_buf, ":443")?,
    };
    tracing::info!("[{}]Handle HTTPS proxy to {} ", tunnel_id, target_addr);

    if direct_ctx
        .try_bypass(tunnel_id, inbound, &target_addr, None)
        .await?
    {
        return Ok(());
    }

    let msg = Message::open_tcp_stream(inbound, target_addr, None);
    sender.send(msg).await?;
    Ok(())
}
```

Note: `headers_buf` becomes partially unused (only used in the `Err` branch of SNI peek). That matches existing code; no warning since it is read.

- [ ] **Step 4: Update `tls_local.rs` (`handle_tls`)**

```rust
pub async fn handle_tls(
    tunnel_id: u32,
    inbound: TcpStream,
    sender: ProxySender,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<()> {
    let target_addr = match peek_sni_v2(&inbound).await {
        Ok(mut sni) => {
            sni.push_str(":443");
            sni
        }
        Err(_) => String::from(""),
    };
    if target_addr.is_empty() {
        tracing::error!("[{}]no sni found ", tunnel_id);
        super::transparent::handle_transparent(tunnel_id, inbound, sender, direct_ctx).await
    } else {
        tracing::info!("[{}]Handle TLS proxy to {} ", tunnel_id, target_addr);
        if direct_ctx
            .try_bypass(tunnel_id, inbound, &target_addr, None)
            .await?
        {
            return Ok(());
        }
        let msg = Message::open_tcp_stream(inbound, target_addr, None);
        sender.send(msg).await?;
        Ok(())
    }
}
```

- [ ] **Step 5: Update `transparent.rs` (`handle_transparent`)**

```rust
pub async fn handle_transparent(
    tunnel_id: u32,
    inbound: TcpStream,
    sender: ProxySender,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<()> {
    let target_addr = match get_original_dst(&inbound) {
        Ok(addr) => addr,
        Err(_) => inbound.local_addr()?,
    };
    let target_addr = target_addr.to_string();
    tracing::info!(
        "[{}]Handle transparent proxy to {} ",
        tunnel_id,
        target_addr
    );

    if direct_ctx
        .try_bypass(tunnel_id, inbound, &target_addr, None)
        .await?
    {
        return Ok(());
    }

    let msg = Message::open_tcp_stream(inbound, target_addr, None);
    sender.send(msg).await?;
    Ok(())
}
```

- [ ] **Step 6: Build and run the full test suite**

Run: `cargo build 2>&1 | tail -30`
Expected: builds cleanly (fix any remaining unused-import / moved-value errors; `headers_buf` in `handle_https` is used in the `Err` arm so it is fine).

Run: `cargo test`
Expected: all tests PASS.

- [ ] **Step 7: Commit**

```bash
git add src/tunnel/local.rs src/tunnel/socks5_local.rs src/tunnel/http_local.rs src/tunnel/tls_local.rs src/tunnel/transparent.rs src/tunnel/direct.rs src/main.rs src/app_config.rs
git commit -m "feat(direct): wire try_bypass into local handlers + Args/AppConfig"
```

---

### Task 8: Wire admin reload to `DirectCtx::reload`

**Files:**
- Modify: `src/admin.rs:324-356` (`handle_config_save`)

- [ ] **Step 1: Add the reload call**

In `src/admin.rs` `handle_config_save`, after `config.trigger_reload().await;` (line 352) and before the final `tracing::info!`:

```rust
    // Re-read direct-bypass rules file (if any) so edits to the rules file
    // take effect on the same admin "save" action.
    let _ = config.direct_ctx.reload();
```

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: builds.

- [ ] **Step 3: Commit**

```bash
git add src/admin.rs
git commit -m "feat(direct): trigger direct-rules reload on admin config save"
```

---

### Task 9: Lint, final test pass, cleanup

**Files:**
- Modify: `src/tunnel/direct.rs` (remove any unused imports)

- [ ] **Step 1: Remove unused imports in `direct.rs`**

Check the top of `src/tunnel/direct.rs`. Remove any of `Ipv4Addr`, `Ipv6Addr`, `AsyncReadExt`, `AsyncWriteExt` that are unused after final wiring. `AsyncWriteExt` is used (`write_all`). `AsyncReadExt` may be unused — remove if so. `Ipv4Addr`/`Ipv6Addr` are unused — remove. Keep `IpAddr`, `SocketAddr`, `IpNet`, `Ipv4Net`, `Ipv6Net`, `TcpStream`, `Stream`, `DEFAULT_TIMEOUT_SECS`, `Arc`, `RwLock`, `Path`.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-features 2>&1 | tail -40`
Expected: no warnings in `direct.rs` or the touched handlers. Fix any that appear.

- [ ] **Step 3: Run the full test suite one more time**

Run: `cargo test`
Expected: all tests PASS.

- [ ] **Step 4: Run fmt check**

Run: `cargo fmt --check`
Expected: no diff. If diff, run `cargo fmt` and re-check.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore(direct): clippy/fmt cleanup"
```

---

## Self-Review

**Spec coverage:**
- CIDR + domain-suffix matching, no DNS → Task 2 (`DirectRules::matches`, `matched_rule`). ✓
- Plain-text file, one rule/line, auto-classified, `*.suffix` vs exact → Task 2 (`parse_lines`). ✓
- `--direct-rules` path arg → Task 6. ✓
- Hot reload (mtime watcher + admin trigger) → Task 5 (`start_direct_watcher`) + Task 8. ✓
- Direct-connect failure closes, no fallback → Task 4 (`try_bypass` returns `Ok(true)` on error). ✓
- TCP only → only TCP handlers touched; UDP untouched. ✓
- Default rules on by default; `--no-direct-bypass`, `--no-default-bypass` → Task 6 + Task 2 (`defaults`). ✓
- Shared `try_bypass` from 4 handlers → Task 7. ✓
- Error matrix (startup missing file → warn+defaults; reload error → keep previous) → Task 3. ✓
- Logging (`Direct bypass hit`, `Direct connect failed`, reload logs) → Task 4 + Task 3. ✓
- Metrics (3 counters/gauges) → Task 4. ✓
- `ipnet` dep → Task 1. ✓
- Tests (parse, matches incl. `notcorp.local`/`*.suffix`-not-self, defaults, host extraction, reload) → Tasks 2/3/4/5. ✓
- Files touched match spec's list. ✓

**Type consistency:** `try_bypass` signature final form is `(tunnel_id: u32, inbound: TcpStream, target_addr: &str, payload: Option<&[u8]>) -> anyhow::Result<bool>` — used consistently in Tasks 4 and 7. `DirectCtx::new(enabled, include_defaults, path: Option<PathBuf>, idle_timeout_secs)` consistent across Tasks 3/6. `DirectRules` fields `cidrs`/`domain_exact`/`domain_suffix` consistent throughout.

**Note on Task 6/7 sequencing:** Task 6 changes `Args`/`AppConfig`/`service_main` and the `start_local_tunnel_server` call; the build is only green again after Task 7's handler signature updates. The plan merges the commit at the end of Task 7 (single commit for the wiring) to avoid a broken intermediate commit.
