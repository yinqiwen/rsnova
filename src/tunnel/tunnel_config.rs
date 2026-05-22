use anyhow::{anyhow, Result};

use crate::mux::event::TunnelEntry;

/// Parse a single --tunnel argument string into a TunnelEntry.
///
/// Formats:
///   port                         -> localhost:port, remote_port=port, sni=None
///   localPort:remotePort         -> localhost:localPort, remote_port=remotePort, sni=None
///   host:localPort:remotePort    -> host:localPort, remote_port=remotePort, sni=None
///   host:localPort:remotePort:sni -> host:localPort, remote_port=remotePort, sni=Some(sni)
///   [ipv6]:localPort:remotePort[:sni] -> IPv6 support
pub fn parse_tunnel_arg(s: &str) -> Result<TunnelEntry> {
    if s.starts_with('[') {
        parse_ipv6_tunnel(s)
    } else {
        parse_simple_tunnel(s)
    }
}

fn parse_ipv6_tunnel(s: &str) -> Result<TunnelEntry> {
    let close_bracket = s
        .find(']')
        .ok_or_else(|| anyhow!("missing closing ']' in IPv6 address"))?;
    let host = &s[1..close_bracket];
    let remainder = &s[close_bracket + 1..];
    if !remainder.starts_with(':') {
        return Err(anyhow!("expected ':' after IPv6 address"));
    }
    let parts: Vec<&str> = remainder[1..].split(':').collect();
    match parts.len() {
        2 => {
            let local_port = parse_port(parts[0])?;
            let remote_port = parse_port(parts[1])?;
            Ok(TunnelEntry {
                local_addr: format!("[{}]:{}", host, local_port),
                remote_port,
                sni: None,
            })
        }
        3 => {
            let local_port = parse_port(parts[0])?;
            let remote_port = parse_port(parts[1])?;
            validate_sni(parts[2])?;
            Ok(TunnelEntry {
                local_addr: format!("[{}]:{}", host, local_port),
                remote_port,
                sni: Some(parts[2].to_string()),
            })
        }
        _ => Err(anyhow!(
            "invalid IPv6 tunnel format: expected [host]:localPort:remotePort[:sni]"
        )),
    }
}

fn parse_simple_tunnel(s: &str) -> Result<TunnelEntry> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => {
            let port = parse_port(parts[0])?;
            Ok(TunnelEntry {
                local_addr: format!("localhost:{}", port),
                remote_port: port,
                sni: None,
            })
        }
        2 => {
            let local_port = parse_port(parts[0])?;
            let remote_port = parse_port(parts[1])?;
            Ok(TunnelEntry {
                local_addr: format!("localhost:{}", local_port),
                remote_port,
                sni: None,
            })
        }
        3 => {
            let host = parts[0];
            let local_port = parse_port(parts[1])?;
            let remote_port = parse_port(parts[2])?;
            Ok(TunnelEntry {
                local_addr: format!("{}:{}", host, local_port),
                remote_port,
                sni: None,
            })
        }
        4 => {
            let host = parts[0];
            let local_port = parse_port(parts[1])?;
            let remote_port = parse_port(parts[2])?;
            validate_sni(parts[3])?;
            Ok(TunnelEntry {
                local_addr: format!("{}:{}", host, local_port),
                remote_port,
                sni: Some(parts[3].to_string()),
            })
        }
        _ => Err(anyhow!("invalid tunnel format: too many ':' segments")),
    }
}

fn parse_port(s: &str) -> Result<u16> {
    let port: u16 = s
        .parse()
        .map_err(|_| anyhow!("invalid port number: '{}'", s))?;
    if port == 0 {
        return Err(anyhow!("port 0 is not allowed"));
    }
    Ok(port)
}

fn validate_sni(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(anyhow!("SNI cannot be empty"));
    }
    if s.contains(':') {
        return Err(anyhow!("SNI must not contain ':'"));
    }
    Ok(())
}

/// Parse --tunnel-port-range (e.g., "8000-9000,10000-10100") into a set of allowed ports.
/// Returns error if any range includes ports < 1024.
pub fn parse_port_range(s: &str) -> Result<Vec<(u16, u16)>> {
    let mut ranges = Vec::new();
    for range_str in s.split(',') {
        let range_str = range_str.trim();
        if range_str.is_empty() {
            continue;
        }
        let parts: Vec<&str> = range_str.split('-').collect();
        if parts.len() != 2 {
            return Err(anyhow!(
                "invalid port range format: '{}', expected 'start-end'",
                range_str
            ));
        }
        let start: u16 = parts[0]
            .parse()
            .map_err(|_| anyhow!("invalid port: '{}'", parts[0]))?;
        let end: u16 = parts[1]
            .parse()
            .map_err(|_| anyhow!("invalid port: '{}'", parts[1]))?;
        if start > end {
            return Err(anyhow!("invalid range: start {} > end {}", start, end));
        }
        if start < 1024 {
            return Err(anyhow!(
                "privileged ports (<1024) not allowed: range starts at {}",
                start
            ));
        }
        ranges.push((start, end));
    }
    if ranges.is_empty() {
        return Err(anyhow!("empty port range"));
    }
    Ok(ranges)
}

/// Check if a port is within the allowed ranges (inclusive).
pub fn is_port_allowed(port: u16, ranges: &[(u16, u16)]) -> bool {
    ranges
        .iter()
        .any(|(start, end)| port >= *start && port <= *end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_single_port() {
        let entry = parse_tunnel_arg("1234").unwrap();
        assert_eq!(entry.local_addr, "localhost:1234");
        assert_eq!(entry.remote_port, 1234);
        assert_eq!(entry.sni, None);
    }

    #[test]
    fn test_parse_local_remote_port() {
        let entry = parse_tunnel_arg("3306:33306").unwrap();
        assert_eq!(entry.local_addr, "localhost:3306");
        assert_eq!(entry.remote_port, 33306);
        assert_eq!(entry.sni, None);
    }

    #[test]
    fn test_parse_host_local_remote() {
        let entry = parse_tunnel_arg("192.168.1.100:3306:33306").unwrap();
        assert_eq!(entry.local_addr, "192.168.1.100:3306");
        assert_eq!(entry.remote_port, 33306);
        assert_eq!(entry.sni, None);
    }

    #[test]
    fn test_parse_host_local_remote_sni() {
        let entry = parse_tunnel_arg("192.168.1.10:8080:443:api.example.com").unwrap();
        assert_eq!(entry.local_addr, "192.168.1.10:8080");
        assert_eq!(entry.remote_port, 443);
        assert_eq!(entry.sni, Some("api.example.com".to_string()));
    }

    #[test]
    fn test_parse_ipv6() {
        let entry = parse_tunnel_arg("[::1]:3306:33306").unwrap();
        assert_eq!(entry.local_addr, "[::1]:3306");
        assert_eq!(entry.remote_port, 33306);
        assert_eq!(entry.sni, None);
    }

    #[test]
    fn test_parse_port_zero_rejected() {
        assert!(parse_tunnel_arg("0").is_err());
    }

    #[test]
    fn test_parse_sni_with_colon_rejected() {
        assert!(parse_tunnel_arg("host:80:443:bad:sni").is_err());
    }

    #[test]
    fn test_port_range_basic() {
        let ranges = parse_port_range("8000-9000").unwrap();
        assert_eq!(ranges, vec![(8000, 9000)]);
        assert!(is_port_allowed(8000, &ranges));
        assert!(is_port_allowed(9000, &ranges));
        assert!(!is_port_allowed(7999, &ranges));
    }

    #[test]
    fn test_port_range_multi() {
        let ranges = parse_port_range("8000-9000,10000-10100").unwrap();
        assert!(is_port_allowed(8500, &ranges));
        assert!(is_port_allowed(10050, &ranges));
        assert!(!is_port_allowed(9500, &ranges));
    }

    #[test]
    fn test_port_range_privileged_rejected() {
        assert!(parse_port_range("80-443").is_err());
    }

    #[test]
    fn test_parse_invalid_port_number() {
        assert!(parse_tunnel_arg("99999").is_err());
    }

    #[test]
    fn test_parse_port_one() {
        let entry = parse_tunnel_arg("1").unwrap();
        assert_eq!(entry.remote_port, 1);
    }

    #[test]
    fn test_parse_sni_with_underscore() {
        let entry = parse_tunnel_arg("host:8080:443:api_dev.example.com").unwrap();
        assert_eq!(entry.sni, Some("api_dev.example.com".to_string()));
    }

    #[test]
    fn test_port_range_empty_string() {
        assert!(parse_port_range("").is_err());
    }

    #[test]
    fn test_parse_empty_sni_rejected() {
        assert!(parse_tunnel_arg("host:80:443:").is_err());
    }
}
