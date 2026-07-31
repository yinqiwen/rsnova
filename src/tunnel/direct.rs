use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::mux::event::OpenStreamError;
use crate::tunnel::DEFAULT_TIMEOUT_SECS;
use crate::tunnel::client::ConnectReply;
use crate::tunnel::stream::Stream;

/// Compiled, immutable rule snapshot. Replaced as a whole on reload (single
/// RwLock → no torn reads between cidrs and domain rules mid-swap).
pub struct DirectRules {
    cidrs: Vec<IpNet>,
    domain_exact: Vec<String>,
    domain_suffix: Vec<String>,
}

impl DirectRules {
    pub fn empty() -> Self {
        Self {
            cidrs: Vec::new(),
            domain_exact: Vec::new(),
            domain_suffix: Vec::new(),
        }
    }

    /// Built-in defaults: loopback, private, link-local, ULA + localhost + *.localhost.
    pub fn defaults() -> Self {
        let cidrs: Vec<IpNet> = [
            "127.0.0.0/8",
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "169.254.0.0/16",
            "::1/128",
            "fc00::/7",
            "fe80::/10",
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
                if rest.is_empty() {
                    tracing::warn!("direct rules: skipping empty wildcard line: {line:?}");
                } else {
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
        Self {
            cidrs,
            domain_exact,
            domain_suffix,
        }
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
        Self {
            cidrs,
            domain_exact,
            domain_suffix,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.cidrs.is_empty() && self.domain_exact.is_empty() && self.domain_suffix.is_empty()
    }

    /// `host` is an IP literal (matched against cidrs) or a domain name
    /// (exact- or suffix-matched). Case-insensitive for domains.
    #[allow(dead_code)]
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
        let h_bytes = h.as_bytes();
        for s in &self.domain_suffix {
            let s_bytes = s.as_bytes();
            // h ends with ".{s}" (a leading dot, so self/sibling false-positives are
            // rejected) — checked without allocating.
            if h_bytes.len() >= s_bytes.len() + 2
                && h_bytes[h_bytes.len() - s_bytes.len() - 1] == b'.'
                && &h_bytes[h_bytes.len() - s_bytes.len()..] == s_bytes
            {
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
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Extract the host portion of a `host:port` / `[ipv6]:port` / `ipv6:port` /
/// bare address.
///
/// Note: SOCKS5 emits unbracketed IPv6+port (e.g. `::1:443`). We deliberately
/// do NOT eagerly `parse::<IpAddr>()` here, because `::1:443` parses as the
/// IPv6 address `0:0:0:0:0:0:1:443` rather than host `::1` + port `443`.
/// Falling through to `rsplit_once(':')` correctly peels the trailing port
/// (a u16 has no `:`), yielding host `::1`.
pub fn extract_host(addr: &str) -> Option<String> {
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        return Some(sa.ip().to_string());
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

/// Shared handle held by the local handlers. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct DirectCtx {
    pub enabled: bool,
    pub include_defaults: bool,
    pub path: Option<PathBuf>,
    pub rules: Arc<RwLock<DirectRules>>,
    pub idle_timeout_secs: usize,
}

impl DirectCtx {
    pub fn new(
        enabled: bool,
        include_defaults: bool,
        path: Option<PathBuf>,
        idle_timeout_secs: usize,
    ) -> Self {
        let rules = Self::load_rules(include_defaults, path.as_deref());
        Self {
            enabled,
            include_defaults,
            path,
            rules: Arc::new(RwLock::new(rules)),
            idle_timeout_secs,
        }
    }

    /// Compose defaults (if enabled) ⊕ file rules. On file IO error at
    /// startup: warn and fall back to defaults-only (or empty if no defaults).
    fn load_rules(include_defaults: bool, path: Option<&Path>) -> DirectRules {
        let base = if include_defaults {
            DirectRules::defaults()
        } else {
            DirectRules::empty()
        };
        let Some(p) = path else {
            return base;
        };
        match DirectRules::parse_file(p) {
            Ok(f) => base.merge(f),
            Err(e) => {
                tracing::warn!("direct rules file load failed, using defaults only: {p:?}: {e}");
                base
            }
        }
    }

    /// Re-read file (if path set) and swap. On file error: warn and keep
    /// previous rules. No-op (Ok) when path is None.
    pub fn reload(&self) -> anyhow::Result<()> {
        let Some(p) = self.path.as_deref() else {
            return Ok(());
        };
        let new = match DirectRules::parse_file(p) {
            Ok(f) => {
                let base = if self.include_defaults {
                    DirectRules::defaults()
                } else {
                    DirectRules::empty()
                };
                base.merge(f)
            }
            Err(e) => {
                tracing::warn!("direct rules reload failed, keeping previous: {p:?}: {e}");
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

impl DirectCtx {
    /// Ok(true)  = handled by direct bypass (success or failure — caller returns Ok(())).
    /// Ok(false) = not matched / disabled — caller proceeds to remote.
    pub async fn try_bypass(
        &self,
        tunnel_id: u32,
        inbound: &mut TcpStream,
        target_addr: &str,
        payload: Option<&[u8]>,
        connect_reply: ConnectReply,
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
        let Some(rule) = rule else {
            return Ok(false);
        };

        tracing::info!("[{tunnel_id}] Direct bypass hit: {target_addr} (rule={rule})");
        metrics::counter!("client_proxy_direct_total").increment(1);

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
                let response =
                    connect_reply.failure_response(&OpenStreamError::Other(e.to_string()));
                let _ = inbound.write_all(&response).await;
                return Ok(true);
            }
            Err(e) => {
                metrics::counter!("client_proxy_direct_connect_failed_total").increment(1);
                tracing::warn!("[{tunnel_id}] Direct connect timed out: {target_addr}: {e}");
                let response = connect_reply.failure_response(&OpenStreamError::TimedOut);
                let _ = inbound.write_all(&response).await;
                return Ok(true);
            }
        };

        metrics::gauge!("client_proxy_direct_streams").increment(1.0);

        if let Err(e) = inbound.write_all(connect_reply.success_response()).await {
            tracing::debug!("[{tunnel_id}] Direct success response failed: {e}");
            metrics::gauge!("client_proxy_direct_streams").decrement(1.0);
            return Ok(true);
        }

        if let Some(p) = payload
            && let Err(e) = outbound.write_all(p).await
        {
            tracing::debug!("[{tunnel_id}] Direct payload write failed: {e}");
            metrics::gauge!("client_proxy_direct_streams").decrement(1.0);
            return Ok(true);
        }

        let (mut in_r, mut in_w) = tokio::io::split(inbound);
        let (mut out_r, mut out_w) = outbound.into_split();
        let mut stream = Stream::new(&mut in_r, &mut in_w, &mut out_r, &mut out_w);
        if let Err(e) = stream.transfer(self.idle_timeout_secs).await {
            tracing::debug!("[{tunnel_id}] Direct transfer finish: {e}");
        }
        metrics::gauge!("client_proxy_direct_streams").decrement(1.0);
        Ok(true)
    }
}

/// Background task: poll `ctx.path`'s mtime every 5s; on change call
/// `ctx.reload()`. Only spawn when `ctx.path` is Some.
pub fn start_direct_watcher(ctx: DirectCtx) {
    let Some(path) = ctx.path.clone() else {
        return;
    };
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
        assert!(
            r.cidrs
                .iter()
                .any(|c| c.contains(&IpAddr::V4("10.5.0.1".parse().unwrap())))
        );
        assert!(
            r.cidrs
                .iter()
                .any(|c| c.contains(&IpAddr::V4("1.2.3.4".parse().unwrap())))
        );
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

    #[test]
    fn suffix_requires_leading_dot_boundary() {
        let r = DirectRules::parse_lines("*.x.com\n");
        assert!(r.matches("a.x.com")); // shortest valid match
        assert!(!r.matches("x.com")); // no leading label
        assert!(!r.matches("bx.com")); // char before suffix is not '.'
        assert!(r.matches("a.b.x.com")); // multi-label
    }

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
    fn ctx_reload_keeps_previous_on_file_error() {
        // Point at a nonexistent path: startup falls back to defaults,
        // reload must KEEP the previous (defaults) rather than clearing.
        let path = PathBuf::from("/nonexistent/rsnova-direct-test-no-such-file.txt");
        let ctx = DirectCtx::new(true, true, Some(path), 30);
        assert!(ctx.rules.read().unwrap().matches("127.0.0.1"));
        let _ = ctx.reload();
        // Still matches defaults → previous rules were retained.
        assert!(ctx.rules.read().unwrap().matches("127.0.0.1"));
    }

    #[test]
    fn extract_host_handles_ip_domain_and_ipv6() {
        assert_eq!(extract_host("127.0.0.1:443"), Some("127.0.0.1".to_string()));
        assert_eq!(extract_host("[::1]:443"), Some("::1".to_string()));
        // SOCKS5 emits unbracketed IPv6+port; the port must be peeled, not
        // parsed as a trailing hextet of the address.
        assert_eq!(extract_host("::1:443"), Some("::1".to_string()));
        assert_eq!(
            extract_host("2001:db8::1:443"),
            Some("2001:db8::1".to_string())
        );
        assert_eq!(
            extract_host("example.com:80"),
            Some("example.com".to_string())
        );
        assert_eq!(extract_host("example.com"), Some("example.com".to_string()));
    }

    /// Regression: SOCKS5 IPv6 loopback target (`::1:443`) must match the
    /// built-in `::1/128` default rule and be direct-bypassed, not silently
    /// fall through to the remote tunnel.
    #[test]
    fn socks5_ipv6_loopback_matches_default() {
        let host = extract_host("::1:443").unwrap();
        assert_eq!(host, "::1");
        assert!(DirectRules::defaults().matches(&host));
    }

    #[tokio::test]
    async fn reload_picks_up_file_changes() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("rsnova-direct-test-{}.txt", std::process::id()));
        std::fs::write(&path, "10.0.0.0/8\n").unwrap();
        let ctx = DirectCtx::new(true, false, Some(path.clone()), 30);
        assert!(ctx.rules.read().unwrap().matches("10.1.2.3"));
        assert!(!ctx.rules.read().unwrap().matches("192.168.1.1"));

        // Rewrite the file; reload; new rule applies, old rule gone (no defaults).
        std::fs::write(&path, "192.168.0.0/16\n").unwrap();
        ctx.reload().unwrap();
        assert!(ctx.rules.read().unwrap().matches("192.168.1.1"));
        assert!(!ctx.rules.read().unwrap().matches("10.1.2.3"));

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn try_bypass_returns_false_for_nonmatching_target() {
        // 8.8.8.8 is not in defaults; with no file, it must not match.
        // try_bypass must return Ok(false) WITHOUT attempting a connect.
        let ctx = DirectCtx::new(true, true, None, 30);
        let acc = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let acc_addr = acc.local_addr().unwrap();
        let mut inbound = tokio::net::TcpStream::connect(acc_addr).await.unwrap();
        drop(acc);
        let handled = ctx
            .try_bypass(0, &mut inbound, "8.8.8.8:9", None, ConnectReply::None)
            .await
            .unwrap();
        assert!(!handled, "non-matching target must not be handled");
    }

    #[tokio::test]
    async fn try_bypass_returns_false_when_disabled() {
        let ctx = DirectCtx::new(false, true, None, 30);
        let acc = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let acc_addr = acc.local_addr().unwrap();
        let mut inbound = tokio::net::TcpStream::connect(acc_addr).await.unwrap();
        drop(acc);
        // Even though 127.0.0.1 would match, disabled must short-circuit.
        let handled = ctx
            .try_bypass(0, &mut inbound, "127.0.0.1:9", None, ConnectReply::None)
            .await
            .unwrap();
        assert!(!handled);
    }
}
