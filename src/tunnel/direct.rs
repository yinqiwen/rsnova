use std::net::IpAddr;
use std::path::Path;

use ipnet::{IpNet, Ipv4Net, Ipv6Net};

/// Compiled, immutable rule snapshot. Replaced as a whole on reload (single
/// RwLock → no torn reads between cidrs and domain rules mid-swap).
pub struct DirectRules {
    cidrs: Vec<IpNet>,
    domain_exact: Vec<String>,
    domain_suffix: Vec<String>,
}

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
        let h_bytes = h.as_bytes();
        for s in &self.domain_suffix {
            let s_bytes = s.as_bytes();
            // h ends with ".{s}" (a leading dot, so self/sibling false-positives are
            // rejected) — checked without allocating.
            if h_bytes.len() >= s_bytes.len() + 2
                && &h_bytes[h_bytes.len() - s_bytes.len() - 1] == &b'.'
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
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
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

    #[test]
    fn suffix_requires_leading_dot_boundary() {
        let r = DirectRules::parse_lines("*.x.com\n");
        assert!(r.matches("a.x.com")); // shortest valid match
        assert!(!r.matches("x.com")); // no leading label
        assert!(!r.matches("bx.com")); // char before suffix is not '.'
        assert!(r.matches("a.b.x.com")); // multi-label
    }
}
