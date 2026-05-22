# NAT Traversal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement reverse tunnel / NAT traversal capability allowing internal network services to be exposed on the public server's ports, supporting both TLS mux and QUIC transport modes.

**Architecture:** Bottom-up layered approach — protocol layer first (new flags + payload types), then CLI parsing, then server-side TunnelRegistry, then auth stream integration into existing connection handlers, then tunnel client accept loop, then visitor accept + reverse stream + relay. QUIC mode last since it's feature-gated and mirrors TLS patterns.

**Tech Stack:** Rust nightly, tokio async runtime, bincode serialization, s2n-quic (feature-gated), tokio-rustls, tokio_util::sync::CancellationToken

**Spec:** `docs/nat_traversal.md`

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/mux/event.rs` | New flag constants (6, 9, 10), auth payload structs, factory functions |
| `src/tunnel/tunnel_config.rs` | `--tunnel` parsing, `TunnelEntry` construction, port-range parsing |
| `src/tunnel/tunnel_registry.rs` | Server-side `TunnelRegistry` + `RouteKey` + `PortState` + `ClientState` |
| `src/tunnel/tunnel_client.rs` | Tunnel client loop: auth stream + accept reverse streams |
| `src/tunnel/tunnel_remote.rs` | Visitor accept loop, SNI routing, reverse stream relay |
| `src/tunnel/tls_remote.rs` (modify) | Auth stream dispatch: proxy vs tunnel mode |
| `src/tunnel/tls_client.rs` (modify) | Add `accept_stream()` to `TlsConnection` |
| `src/tunnel/s2n_quic_client.rs` (modify) | QUIC tunnel client with `Connection::split()` |
| `src/tunnel/s2n_quic_remote.rs` (modify) | QUIC auth dispatch + Handle storage |
| `src/tunnel/client.rs` (modify) | Uncomment `accept_stream()` in `MuxConnection` trait |
| `src/tunnel/mod.rs` (modify) | Module declarations + re-exports |
| `src/main.rs` (modify) | New CLI args, tunnel mode dispatch |

---

## Task 1: Protocol Layer — New Flags and Payload Types

**Files:**
- Modify: `src/mux/event.rs`

- [ ] **Step 1: Add new flag constants**

Add after `FLAG_SHUTDOWN` (line 14):

```rust
// Value 6 was previously unused. FLAG_WIN_UPDATE (4) and FLAG_PONG (8) and
// FLAG_ROUTINE (9) were commented out and their values are being reclaimed:
//   6 → FLAG_AUTH (new), 9 → FLAG_AUTH_ACK (replaces commented-out FLAG_ROUTINE),
//   10 → FLAG_REVERSE_OPEN (new)
pub const FLAG_AUTH: u8 = 6;
pub const FLAG_AUTH_ACK: u8 = 9;
pub const FLAG_REVERSE_OPEN: u8 = 10;
```

> **Important:** When adding these constants, also remove or clearly annotate the
> commented-out `FLAG_PONG = 8` and `FLAG_ROUTINE = 9` lines to make the value
> reassignment explicit. No runtime impact, but avoids future confusion.

- [ ] **Step 2: Add auth payload structs**

Add after `OpenStreamEvent` (after line 70):

```rust
#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub enum AuthRequest {
    Proxy,
    Register(RegisterRequest),
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub struct RegisterRequest {
    pub client_id: String,
    pub tunnels: Vec<TunnelEntry>,
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub struct TunnelEntry {
    pub local_addr: String,
    pub remote_port: u16,
    pub sni: Option<String>,
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub enum AuthAck {
    Proxy,
    RegisterAck(RegisterAck),
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub struct RegisterAck {
    pub results: Vec<TunnelResult>,
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub struct TunnelResult {
    pub success: bool,
    pub remote_port: u16,
    pub sni: Option<String>,
    pub error: Option<String>,
}
```

- [ ] **Step 3: Add factory functions**

Add after `new_open_stream_event()`:

```rust
pub fn new_auth_event(sid: u32, req: &AuthRequest) -> anyhow::Result<Event> {
    let config = config::standard();
    let data: Vec<u8> = bincode::encode_to_vec(req, config)
        .map_err(|e| anyhow::anyhow!("encode auth request failed: {}", e))?;
    let mut ev = new_event(sid, Bytes::from(data));
    ev.header.set_flag(FLAG_AUTH);
    Ok(ev)
}

pub fn new_auth_ack_event(sid: u32, ack: &AuthAck) -> anyhow::Result<Event> {
    let config = config::standard();
    let data: Vec<u8> = bincode::encode_to_vec(ack, config)
        .map_err(|e| anyhow::anyhow!("encode auth ack failed: {}", e))?;
    let mut ev = new_event(sid, Bytes::from(data));
    ev.header.set_flag(FLAG_AUTH_ACK);
    Ok(ev)
}

pub fn new_reverse_open_stream_event(sid: u32, msg: &OpenStreamEvent) -> anyhow::Result<Event> {
    let config = config::standard();
    let data: Vec<u8> = bincode::encode_to_vec(msg, config)
        .map_err(|e| anyhow::anyhow!("encode reverse open stream event failed: {}", e))?;
    let mut ev = new_event(sid, Bytes::from(data));
    ev.header.set_flag(FLAG_REVERSE_OPEN);
    Ok(ev)
}
```

- [ ] **Step 4: Verify compilation**

Run: `cargo build 2>&1 | head -20`
Expected: Build succeeds (new code is additive, no breaking changes)

- [ ] **Step 5: Commit**

```bash
git add src/mux/event.rs
git commit -m "feat(mux): add FLAG_AUTH/FLAG_AUTH_ACK/FLAG_REVERSE_OPEN and auth payload types"
```

---

## Task 2: Tunnel Config — CLI Parsing and TunnelEntry Construction

**Files:**
- Create: `src/tunnel/tunnel_config.rs`

- [ ] **Step 1: Create tunnel_config.rs with --tunnel parsing**

```rust
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
    let close_bracket = s.find(']').ok_or_else(|| anyhow!("missing closing ']' in IPv6 address"))?;
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
        _ => Err(anyhow!("invalid IPv6 tunnel format: expected [host]:localPort:remotePort[:sni]")),
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
    let port: u16 = s.parse().map_err(|_| anyhow!("invalid port number: '{}'", s))?;
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
            return Err(anyhow!("invalid port range format: '{}', expected 'start-end'", range_str));
        }
        let start: u16 = parts[0].parse().map_err(|_| anyhow!("invalid port: '{}'", parts[0]))?;
        let end: u16 = parts[1].parse().map_err(|_| anyhow!("invalid port: '{}'", parts[1]))?;
        if start > end {
            return Err(anyhow!("invalid range: start {} > end {}", start, end));
        }
        if start < 1024 {
            return Err(anyhow!("privileged ports (<1024) not allowed: range starts at {}", start));
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
    ranges.iter().any(|(start, end)| port >= *start && port <= *end)
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
        assert!(parse_tunnel_arg("99999").is_err()); // > u16::MAX
    }

    #[test]
    fn test_parse_port_one() {
        // Port 1 is valid on client side (server-side range check rejects <1024)
        let entry = parse_tunnel_arg("1").unwrap();
        assert_eq!(entry.remote_port, 1);
    }

    #[test]
    fn test_parse_sni_with_underscore() {
        // Underscore is technically invalid per RFC 1034 but commonly used
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
```

- [ ] **Step 2: Register module in tunnel/mod.rs**

Add to `src/tunnel/mod.rs`:

```rust
pub mod tunnel_config;
```

- [ ] **Step 3: Run tests**

Run: `cargo test tunnel_config -- --nocapture`
Expected: All 14 tests pass

- [ ] **Step 4: Commit**

```bash
git add src/tunnel/tunnel_config.rs src/tunnel/mod.rs
git commit -m "feat(tunnel): add --tunnel and --tunnel-port-range parsing with tests"
```

---

## Task 3: TunnelRegistry — Server-Side State Management

**Files:**
- Create: `src/tunnel/tunnel_registry.rs`

- [ ] **Step 1: Create TunnelRegistry with core types and register/unregister logic**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::mux::event::TunnelEntry;

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct RouteKey {
    pub remote_port: u16,
    pub sni: Option<String>,
}

pub struct ActiveTunnel {
    pub client_id: String,
    pub local_addr: String,
    pub remote_port: u16,
    pub sni: Option<String>,
}

pub struct PortState {
    pub listener: Arc<TcpListener>,
    pub listener_handle: JoinHandle<()>,
    pub cancel_token: CancellationToken,
    pub active_routes: Vec<RouteKey>,
}

/// Abstraction over TLS mux::Connection and QUIC Handle for opening reverse streams.
/// Both support concurrent open_stream() from multiple tasks.
/// Clone is needed so the handler can be cloned out of the registry lock before async I/O.
#[derive(Clone)]
pub enum ConnectionHandler {
    Tls(Arc<crate::mux::Connection>),
    #[cfg(feature = "s2n_quic")]
    Quic(s2n_quic::connection::Handle),
}

pub struct ClientConnection {
    pub handler: ConnectionHandler,
    pub conn_id: u32,
}

pub struct ClientState {
    pub connections: Vec<ClientConnection>,
    pub routes: Vec<RouteKey>,
    pub cursor: usize,
}

impl ClientState {
    /// Round-robin select a connection from the pool.
    pub fn next_connection(&mut self) -> Option<&ConnectionHandler> {
        if self.connections.is_empty() {
            return None;
        }
        let idx = self.cursor % self.connections.len();
        self.cursor = self.cursor.wrapping_add(1);
        Some(&self.connections[idx].handler)
    }
}

pub struct TunnelRegistry {
    pub routes: HashMap<RouteKey, ActiveTunnel>,
    pub ports: HashMap<u16, PortState>,
    pub clients: HashMap<String, ClientState>,
    /// Ports that server itself uses (--listen, --admin-listen), to prevent self-loop.
    pub reserved_ports: Vec<u16>,
    /// Allowed port ranges from --tunnel-port-range.
    pub allowed_ranges: Vec<(u16, u16)>,
}

impl TunnelRegistry {
    pub fn new(allowed_ranges: Vec<(u16, u16)>, reserved_ports: Vec<u16>) -> Self {
        Self {
            routes: HashMap::new(),
            ports: HashMap::new(),
            clients: HashMap::new(),
            reserved_ports,
            allowed_ranges,
        }
    }

    /// Validate a tunnel entry. Returns error string if invalid.
    pub fn validate_entry(&self, entry: &TunnelEntry) -> Option<String> {
        use crate::tunnel::tunnel_config::is_port_allowed;

        if entry.remote_port == 0 {
            return Some("remote_port=0 is not allowed".to_string());
        }
        if !is_port_allowed(entry.remote_port, &self.allowed_ranges) {
            return Some(format!("port {} not in allowed range", entry.remote_port));
        }
        if self.reserved_ports.contains(&entry.remote_port) {
            return Some(format!("port {} is reserved by server", entry.remote_port));
        }
        let key = RouteKey {
            remote_port: entry.remote_port,
            sni: entry.sni.clone(),
        };
        if let Some(existing) = self.routes.get(&key) {
            return Some(format!(
                "port {}:{} already registered by client '{}'",
                entry.remote_port,
                entry.sni.as_deref().unwrap_or("default"),
                existing.client_id
            ));
        }
        None
    }

    /// Register a tunnel route. Caller must bind the port if this is the first route on it.
    pub fn register_route(&mut self, client_id: &str, entry: &TunnelEntry) {
        let key = RouteKey {
            remote_port: entry.remote_port,
            sni: entry.sni.clone(),
        };
        self.routes.insert(
            key.clone(),
            ActiveTunnel {
                client_id: client_id.to_string(),
                local_addr: entry.local_addr.clone(),
                remote_port: entry.remote_port,
                sni: entry.sni.clone(),
            },
        );
        // Track route under client
        let client_state = self.clients.entry(client_id.to_string()).or_insert_with(|| ClientState {
            connections: Vec::new(),
            routes: Vec::new(),
            cursor: 0,
        });
        if !client_state.routes.contains(&key) {
            client_state.routes.push(key.clone());
        }
        // Track route under port
        if let Some(port_state) = self.ports.get_mut(&entry.remote_port) {
            if !port_state.active_routes.contains(&key) {
                port_state.active_routes.push(key);
            }
        }
    }

    /// Remove all routes for a client. Returns ports that have no remaining routes (should be unbound).
    pub fn remove_client_routes(&mut self, client_id: &str) -> Vec<u16> {
        let mut empty_ports = Vec::new();
        if let Some(client_state) = self.clients.get(client_id) {
            let routes_to_remove: Vec<RouteKey> = client_state.routes.clone();
            for key in &routes_to_remove {
                self.routes.remove(key);
                if let Some(port_state) = self.ports.get_mut(&key.remote_port) {
                    port_state.active_routes.retain(|r| r != key);
                    if port_state.active_routes.is_empty() {
                        empty_ports.push(key.remote_port);
                    }
                }
            }
        }
        self.clients.remove(client_id);
        empty_ports
    }

    /// Remove a single connection from a client's pool. Returns true if client has no connections left.
    pub fn remove_connection(&mut self, client_id: &str, conn_id: u32) -> bool {
        if let Some(client_state) = self.clients.get_mut(client_id) {
            client_state.connections.retain(|c| c.conn_id != conn_id);
            client_state.connections.is_empty()
        } else {
            true
        }
    }

    /// Add a connection to a client's pool.
    pub fn add_connection(&mut self, client_id: &str, conn: ClientConnection) {
        let client_state = self.clients.entry(client_id.to_string()).or_insert_with(|| ClientState {
            connections: Vec::new(),
            routes: Vec::new(),
            cursor: 0,
        });
        client_state.connections.push(conn);
    }

    /// Lookup a route by (remote_port, sni). Falls back to (remote_port, None) if SNI not found.
    pub fn lookup_route(&self, remote_port: u16, sni: Option<&str>) -> Option<&ActiveTunnel> {
        if let Some(sni_str) = sni {
            let key = RouteKey { remote_port, sni: Some(sni_str.to_string()) };
            if let Some(tunnel) = self.routes.get(&key) {
                return Some(tunnel);
            }
        }
        // Fallback to default route (no SNI)
        let default_key = RouteKey { remote_port, sni: None };
        self.routes.get(&default_key)
    }

    /// Check if a port already has a listener registered.
    pub fn has_port(&self, port: u16) -> bool {
        self.ports.contains_key(&port)
    }
}

/// Shared registry type. Uses Mutex (not RwLock) because:
/// - Lock hold time is very short (clone a handler reference + update cursor, no I/O)
/// - All write paths (register/unregister) need exclusive access anyway
/// - RwLock overhead (reader tracking) isn't justified when locks are held briefly
pub type SharedRegistry = Arc<Mutex<TunnelRegistry>>;
```

- [ ] **Step 2: Register module in tunnel/mod.rs**

Add to `src/tunnel/mod.rs`:

```rust
pub mod tunnel_registry;
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build 2>&1 | head -20`
Expected: Build succeeds

- [ ] **Step 4: Commit**

```bash
git add src/tunnel/tunnel_registry.rs src/tunnel/mod.rs
git commit -m "feat(tunnel): add TunnelRegistry for server-side route and connection management"
```

---

## Task 4: CLI Arguments — main.rs Integration

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Add new CLI arguments to Args struct**

Add after the `admin_listen` field (around line 146):

```rust
    /// Tunnel entries for NAT traversal (client mode).
    /// Formats: port | localPort:remotePort | host:localPort:remotePort | host:localPort:remotePort:sni
    #[default(Vec::new())]
    #[arg(long = "tunnel", conflicts_with_all = ["listen", "tproxy"])]
    tunnel: Vec<String>,

    /// Client identifier for tunnel mode (required when --tunnel is specified)
    #[default(String::new())]
    #[arg(long = "tunnel-client-id")]
    tunnel_client_id: String,

    /// Server-side allowed port ranges for tunnel (e.g., "8000-9000,10000-10100")
    #[default(String::new())]
    #[arg(long = "tunnel-port-range")]
    tunnel_port_range: String,
```

- [ ] **Step 2: Add tunnel mode validation in service_main()**

Before the `match args.role` block, add validation:

```rust
    // Validate tunnel mode arguments
    let tunnel_entries = if !args.tunnel.is_empty() {
        if args.tunnel_client_id.is_empty() {
            return Err(anyhow!("--tunnel-client-id is required when --tunnel is specified"));
        }
        let mut entries = Vec::new();
        for t in &args.tunnel {
            entries.push(tunnel::tunnel_config::parse_tunnel_arg(t)?);
        }
        Some(entries)
    } else {
        None
    };

    let tunnel_port_ranges = if !args.tunnel_port_range.is_empty() {
        Some(tunnel::tunnel_config::parse_port_range(&args.tunnel_port_range)?)
    } else {
        None
    };
```

- [ ] **Step 3: Add tunnel client branch in Role::Client**

Inside `Role::Client`, before the existing proxy logic, add:

```rust
        Role::Client => {
            if let Some(entries) = tunnel_entries {
                // Tunnel mode: independent flow, does not use proxy mux_client_loop
                tracing::info!("Starting in tunnel mode with client_id: {}", args.tunnel_client_id);
                tunnel::start_tunnel_client(
                    args.remote.as_ref().unwrap(),
                    &args.cert,
                    &args.tls_host,
                    args.idle_timeout_secs,
                    args.mux_stream_channel_size,
                    &args.tunnel_client_id,
                    entries,
                ).await?;
                return Ok(());
            }

            // Existing proxy mode below...
```

> **Note:** `start_tunnel_client` is defined as a stub in Task 5 Step 5. To allow
> this task to compile independently, create a minimal stub in `src/tunnel/mod.rs` now:
>
> ```rust
> pub async fn start_tunnel_client(
>     _url: &url::Url,
>     _cert_path: &std::path::Path,
>     _host: &str,
>     _idle_timeout_secs: usize,
>     _stream_channel_size: usize,
>     _client_id: &str,
>     _entries: Vec<crate::mux::event::TunnelEntry>,
> ) -> anyhow::Result<()> {
>     Err(anyhow::anyhow!("tunnel client not yet implemented"))
> }
> ```
>
> This stub will be replaced with the real implementation in Task 5.

- [ ] **Step 4: Pass port ranges to server startup**

Modify the `Role::Server` branch to pass tunnel configuration:

```rust
        Role::Server => {
            match args.protocol {
                Protocol::Quic => {
                    // existing quic server start, add tunnel_port_ranges parameter
                }
                Protocol::Tls => {
                    // existing tls server start, add tunnel_port_ranges parameter
                }
            }
        }
```

(Exact integration will be done when tunnel_remote.rs is ready in Task 7.)

- [ ] **Step 5: Add necessary imports**

At the top of main.rs, the `tunnel` module is already imported. Ensure the new sub-modules are accessible via `tunnel::tunnel_config`.

- [ ] **Step 6: Verify compilation**

Run: `cargo build 2>&1 | head -30`
Expected: Build succeeds (the `start_tunnel_client` stub in tunnel/mod.rs resolves the import)

- [ ] **Step 7: Commit**

```bash
git add src/main.rs
git commit -m "feat(cli): add --tunnel, --tunnel-client-id, --tunnel-port-range arguments"
```

---

## Task 5: Auth Stream — TLS Client Side (Proxy + Tunnel)

**Files:**
- Modify: `src/tunnel/client.rs` — uncomment `accept_stream()` in trait
- Modify: `src/tunnel/tls_client.rs` — implement `accept_stream()` for `TlsConnection`
- Create: `src/tunnel/tunnel_client.rs` — tunnel client main loop

- [ ] **Step 1: Add accept_stream() to MuxConnection trait**

In `src/tunnel/client.rs`, add to the `MuxConnection` trait (around line 84-87 where it's commented out):

```rust
    async fn accept_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;
```

- [ ] **Step 2: Implement accept_stream() for TlsConnection**

In `src/tunnel/tls_client.rs`, add to the `impl MuxConnection for TlsConnection` block:

```rust
    async fn accept_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)> {
        match &mut self.inner {
            None => Err(anyhow!("null connection")),
            Some(c) => match c.accept_stream().await {
                Ok(stream) => {
                    let (r, w) = tokio::io::split(stream);
                    Ok((w, r))
                }
                Err(e) => {
                    self.inner = None;
                    Err(e)
                }
            },
        }
    }
```

- [ ] **Step 3: Implement accept_stream() for S2NQuicConnection (stub)**

In `src/tunnel/s2n_quic_client.rs`, add stub (will be properly implemented in Task 9):

```rust
    async fn accept_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)> {
        Err(anyhow!("QUIC accept_stream not yet implemented for tunnel mode"))
    }
```

- [ ] **Step 4: Create tunnel_client.rs with auth stream + accept loop**

```rust
use anyhow::{anyhow, Result};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use url::Url;

use crate::mux::event::{
    self, AuthAck, AuthRequest, OpenStreamEvent, RegisterRequest, TunnelEntry,
    FLAG_AUTH_ACK, FLAG_REVERSE_OPEN,
};
use crate::tunnel::stream::Stream;
use crate::tunnel::tls_client::TlsConnection;
use crate::tunnel::client::MuxConnection;

/// Entry point for tunnel client mode (TLS).
pub async fn start_tunnel_client_tls(
    url: &Url,
    cert_path: &Path,
    host: &str,
    idle_timeout_secs: usize,
    stream_channel_size: usize,
    client_id: &str,
    entries: Vec<TunnelEntry>,
) -> Result<()> {
    // Build connections and run auth + accept loop
    // TODO: Support --concurrent=N: spawn N run_tunnel_connection_tls tasks sharing the same client_id
    loop {
        // run_tunnel_connection_tls runs until connection drops (accept_stream errors),
        // so it always returns Err. The Ok branch is unreachable but handled for safety.
        let result = run_tunnel_connection_tls(
            url, cert_path, host, stream_channel_size, client_id, &entries, idle_timeout_secs,
        ).await;
        tracing::info!("Tunnel connection lost ({}), reconnecting in 3s...", result.err().unwrap_or_default());
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn run_tunnel_connection_tls(
    url: &Url,
    cert_path: &Path,
    host: &str,
    stream_channel_size: usize,
    client_id: &str,
    entries: &[TunnelEntry],
    idle_timeout_secs: usize,
) -> Result<()> {
    let mut conn = TlsConnection::new(stream_channel_size);
    conn.connect(url, cert_path, host).await?;
    tracing::info!("TLS tunnel connection established");

    // Phase 1: Auth stream
    let (mut send, mut recv) = conn.open_stream().await?;
    let auth_req = AuthRequest::Register(RegisterRequest {
        client_id: client_id.to_string(),
        tunnels: entries.to_vec(),
    });
    let ev = event::new_auth_event(0, &auth_req)?;
    event::write_event(&mut send, ev).await?;

    // Read auth ack
    let ack_ev = event::read_event(&mut recv).await?;
    if ack_ev.header.flags() != FLAG_AUTH_ACK {
        return Err(anyhow!("expected FLAG_AUTH_ACK, got flag={}", ack_ev.header.flags()));
    }
    let config = bincode::config::standard();
    let (ack, _): (AuthAck, usize) = bincode::decode_from_slice(ack_ev.body.as_ref(), config)
        .map_err(|e| anyhow!("decode AuthAck failed: {}", e))?;
    match ack {
        AuthAck::Proxy => return Err(anyhow!("server returned Proxy ack for tunnel request")),
        AuthAck::RegisterAck(register_ack) => {
            handle_register_ack(&register_ack)?;
        }
    }
    // Auth stream done — drop send/recv halves
    drop(send);
    drop(recv);

    // Phase 2: Accept reverse streams
    tracing::info!("Tunnel client ready, waiting for reverse streams...");
    loop {
        let (mut stream_send, mut stream_recv) = conn.accept_stream().await?;

        tokio::spawn(async move {
            if let Err(e) = handle_reverse_stream(&mut stream_recv, &mut stream_send, idle_timeout_secs).await {
                tracing::warn!("Reverse stream error: {}", e);
            }
        });
    }
}

pub async fn handle_reverse_stream<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
    recv: &mut R,
    send: &mut W,
    idle_timeout_secs: usize,
) -> Result<()> {
    // Read first event: expect FLAG_REVERSE_OPEN
    let ev = event::read_event(recv).await?;
    if ev.header.flags() != FLAG_REVERSE_OPEN {
        return Err(anyhow!("expected FLAG_REVERSE_OPEN, got flag={}", ev.header.flags()));
    }
    let config = bincode::config::standard();
    let (open_event, _): (OpenStreamEvent, usize) =
        bincode::decode_from_slice(ev.body.as_ref(), config)
            .map_err(|e| anyhow!("decode OpenStreamEvent failed: {}", e))?;

    tracing::info!("Reverse stream: connecting to {}", open_event.addr);

    // Connect to local service
    let timeout_dur = Duration::from_secs(30);
    let mut local_stream = tokio::time::timeout(
        timeout_dur,
        tokio::net::TcpStream::connect(&open_event.addr),
    ).await
        .map_err(|_| anyhow!("connect to {} timed out", open_event.addr))?
        .map_err(|e| anyhow!("connect to {} failed: {}", open_event.addr, e))?;

    // Bidirectional relay
    let (mut local_r, mut local_w) = local_stream.split();
    let mut stream = Stream::new(&mut local_r, &mut local_w, recv, send);
    stream.transfer(idle_timeout_secs).await?;
    Ok(())
}

/// Shared handler for RegisterAck — used by both TLS and QUIC tunnel clients.
/// Logs each result and returns Err if all tunnels failed.
pub fn handle_register_ack(ack: &RegisterAck) -> Result<()> {
    let mut any_success = false;
    for result in &ack.results {
        if result.success {
            any_success = true;
            tracing::info!(
                "Tunnel registered: :{}{} → OK",
                result.remote_port,
                result.sni.as_ref().map(|s| format!(" (SNI: {})", s)).unwrap_or_default()
            );
        } else {
            tracing::error!(
                "Tunnel registration failed: :{}{} — {}",
                result.remote_port,
                result.sni.as_ref().map(|s| format!(" (SNI: {})", s)).unwrap_or_default(),
                result.error.as_deref().unwrap_or("unknown error")
            );
        }
    }
    if !any_success {
        Err(anyhow!("all tunnel registrations failed"))
    } else {
        Ok(())
    }
}
```

- [ ] **Step 5: Add public entry point and module registration**

In `src/tunnel/mod.rs`, add:

```rust
pub mod tunnel_client;

pub use self::tunnel_client::start_tunnel_client_tls;
#[cfg(feature = "s2n_quic")]
pub use self::s2n_quic_client::start_tunnel_client_quic;
```

Add a dispatcher function (in `tunnel_client.rs` or a new top-level in mod.rs):

```rust
// In tunnel/mod.rs or as a pub fn in tunnel_client.rs:
pub async fn start_tunnel_client(
    url: &url::Url,
    cert_path: &std::path::Path,
    host: &str,
    idle_timeout_secs: usize,
    stream_channel_size: usize,
    client_id: &str,
    entries: Vec<crate::mux::event::TunnelEntry>,
) -> anyhow::Result<()> {
    match url.scheme() {
        "tls" => {
            tunnel_client::start_tunnel_client_tls(
                url, cert_path, host, idle_timeout_secs,
                stream_channel_size, client_id, entries,
            ).await
        }
        #[cfg(feature = "s2n_quic")]
        "quic" => {
            // Will be implemented in Task 9
            Err(anyhow::anyhow!("QUIC tunnel client not yet implemented"))
        }
        _ => Err(anyhow::anyhow!("unsupported scheme: {}", url.scheme())),
    }
}
```

- [ ] **Step 6: Verify TlsConnection can be constructed without exposing internal functions**

The `run_tunnel_connection_tls` function constructs `TlsConnection { inner: None, id: 0, stream_channel_size }`
directly and calls `conn.connect()` — this is the same pattern used internally by `new_tls_client`.
No need to make `new_tls_connection` public. Verify that `TlsConnection` struct fields are accessible
(they may need `pub` visibility or a constructor method).

If the fields are private, add a simple constructor instead of exposing the raw TLS connection function:

```rust
impl TlsConnection {
    pub fn new(stream_channel_size: usize) -> Self {
        Self { inner: None, id: 0, stream_channel_size }
    }
}
```

- [ ] **Step 7: Verify compilation**

Run: `cargo build 2>&1 | head -30`
Expected: Builds (server side auth handling not yet implemented, but client side compiles)

- [ ] **Step 8: Commit**

```bash
git add src/tunnel/client.rs src/tunnel/tls_client.rs src/tunnel/tunnel_client.rs src/tunnel/mod.rs src/tunnel/s2n_quic_client.rs
git commit -m "feat(tunnel): implement tunnel client with auth stream and reverse stream accept loop"
```

---

## Task 6: Auth Stream — TLS Server Side Dispatch

**Files:**
- Modify: `src/tunnel/tls_remote.rs`

- [ ] **Step 1: Add auth stream dispatch to handle_tls_connection**

> **Important:** Do NOT rewrite the entire function. Only add the auth stream dispatch wrapper
> around the EXISTING proxy accept loop. The existing proxy mode code (metrics gauges,
> stream_id logging, error handling) must be preserved exactly as-is.

The change is: wrap the existing `loop { accept_stream() }` inside an auth stream dispatch.
The new function body should be:

```rust
pub(crate) async fn handle_tls_connection<T: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    conn: T,
    id: u32,
    idle_timeout_secs: usize,
    stream_channel_size: usize,
    registry: Option<crate::tunnel::tunnel_registry::SharedRegistry>,
) -> Result<()> {
    let (r, w) = tokio::io::split(conn);
    let mux_conn = Arc::new(mux::Connection::new_with_stream_channel_size(
        r,
        w,
        mux::Mode::Server,
        id,
        stream_channel_size,
    ));

    // Phase 1: Accept auth stream (first stream)
    let auth_stream = mux_conn.accept_stream().await?;
    let (mut auth_r, mut auth_w) = tokio::io::split(auth_stream);

    let ev = event::read_event(&mut auth_r).await?;
    if ev.header.flags() != event::FLAG_AUTH {
        return Err(anyhow!("expected FLAG_AUTH on first stream, got flag={}", ev.header.flags()));
    }

    let config = bincode::config::standard();
    let (auth_req, _): (event::AuthRequest, usize) =
        bincode::decode_from_slice(ev.body.as_ref(), config)
            .map_err(|e| anyhow!("decode AuthRequest failed: {}", e))?;

    match auth_req {
        event::AuthRequest::Proxy => {
            // Reply with AuthAck::Proxy
            let ack = event::AuthAck::Proxy;
            let ack_ev = event::new_auth_ack_event(0, &ack)?;
            event::write_event(&mut auth_w, ack_ev).await?;
            drop(auth_r);
            drop(auth_w);

            // === EXISTING PROXY MODE CODE — preserved exactly as-is ===
            loop {
                let stream = mux_conn.accept_stream().await?;
                metrics::increment_gauge!("tls_server_proxy_streams", 1.0);
                tokio::spawn(async move {
                    let stream_id = stream.id();
                    let (mut stream_reader, mut stream_writer) = tokio::io::split(stream);
                    if let Err(e) =
                        handle_server_stream(&mut stream_reader, &mut stream_writer, idle_timeout_secs)
                            .await
                    {
                        tracing::error!(
                            "[{}/{}]failed: {reason}",
                            id,
                            stream_id,
                            reason = e.to_string()
                        );
                    }
                    metrics::decrement_gauge!("tls_server_proxy_streams", 1.0);
                });
            }
            // === END EXISTING CODE ===
        }
        event::AuthRequest::Register(register_req) => {
            let Some(registry) = registry else {
                // No tunnel support configured
                let ack = event::AuthAck::RegisterAck(event::RegisterAck {
                    results: register_req.tunnels.iter().map(|t| event::TunnelResult {
                        success: false,
                        remote_port: t.remote_port,
                        sni: t.sni.clone(),
                        error: Some("tunnel not enabled on server".to_string()),
                    }).collect(),
                });
                let ack_ev = event::new_auth_ack_event(0, &ack)?;
                event::write_event(&mut auth_w, ack_ev).await?;
                return Ok(());
            };

            // Tunnel mode: validate and register
            let results = crate::tunnel::tunnel_remote::handle_tunnel_register(
                &registry,
                &register_req,
                crate::tunnel::tunnel_registry::ConnectionHandler::Tls(mux_conn.clone()),
                id,
                idle_timeout_secs,
            ).await;

            let ack = event::AuthAck::RegisterAck(event::RegisterAck { results });
            let ack_ev = event::new_auth_ack_event(0, &ack)?;
            event::write_event(&mut auth_w, ack_ev).await?;
            drop(auth_r);
            drop(auth_w);

            tracing::info!("[{}] Tunnel client '{}' registered, waiting for disconnect...", id, register_req.client_id);

            // Tunnel mode: detect disconnect by accept_stream() returning error.
            // This is preferable to manual ping loops — accept_stream() will error
            // when the underlying connection drops, matching the existing pattern.
            while let Ok(_stream) = mux_conn.accept_stream().await {
                // In tunnel mode, server should not receive client-initiated streams
                // after auth. If we get one, log and discard.
                tracing::warn!("[{}] Unexpected stream in tunnel mode, discarding", id);
            }

            // Connection lost — cleanup
            tracing::info!("[{}] Tunnel client '{}' disconnected, cleaning up", id, register_req.client_id);
            // TODO(metrics): decrement tunnel_active_connections for this client_id
            let mut reg = registry.lock().await;
            let no_connections = reg.remove_connection(&register_req.client_id, id);
            if no_connections {
                let empty_ports = reg.remove_client_routes(&register_req.client_id);
                for port in empty_ports {
                    if let Some(port_state) = reg.ports.remove(&port) {
                        port_state.cancel_token.cancel();
                        tracing::info!("Closed listener on port {}", port);
                    }
                }
            }
            Ok(())
        }
    }
}
```

- [ ] **Step 2: Update function signature in start_tls_remote_server**

Add the `registry` parameter to the server startup and pass it to `handle_tls_connection`:

```rust
pub async fn start_tls_remote_server(
    listen: &SocketAddr,
    cert_path: &Path,
    key_path: &Path,
    idle_timeout_secs: usize,
    stream_channel_size: usize,
    registry: Option<crate::tunnel::tunnel_registry::SharedRegistry>,
) -> Result<()> {
    // ... existing TLS setup ...
    // In the accept loop, pass registry.clone() to handle_tls_connection
}
```

- [ ] **Step 3: Add required imports**

```rust
use std::sync::Arc;
use std::time::Duration;
use crate::mux::event;
```

- [ ] **Step 4: Verify compilation**

Run: `cargo build 2>&1 | head -30`
Expected: May need `tunnel_remote::handle_tunnel_register` stub — create it in the next task.

- [ ] **Step 5: Commit**

```bash
git add src/tunnel/tls_remote.rs
git commit -m "feat(tunnel): add auth stream dispatch in TLS server (proxy vs tunnel mode)"
```

---

## Task 7: Tunnel Remote — Visitor Accept, SNI Routing, Reverse Stream

**Files:**
- Create: `src/tunnel/tunnel_remote.rs`

- [ ] **Step 1: Create tunnel_remote.rs with register handler and visitor accept loop**

```rust
use std::sync::Arc;
use anyhow::{anyhow, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::mux::event::{
    self, OpenStreamEvent, RegisterRequest, TunnelResult, FLAG_REVERSE_OPEN,
};
use crate::tunnel::stream::Stream;
use crate::tunnel::tls_local::peek_sni_v2;
use crate::tunnel::tunnel_registry::{
    ClientConnection, ConnectionHandler, PortState, RouteKey, SharedRegistry,
};

/// Handle tunnel registration: validate entries, bind ports, store connection.
pub async fn handle_tunnel_register(
    registry: &SharedRegistry,
    req: &RegisterRequest,
    handler: ConnectionHandler,
    conn_id: u32,
    idle_timeout_secs: usize,
) -> Vec<TunnelResult> {
    let mut results = Vec::new();
    let mut reg = registry.lock().await;

    for entry in &req.tunnels {
        if let Some(err) = reg.validate_entry(entry) {
            results.push(TunnelResult {
                success: false,
                remote_port: entry.remote_port,
                sni: entry.sni.clone(),
                error: Some(err),
            });
            continue;
        }

        // Bind port if not already bound
        if !reg.has_port(entry.remote_port) {
            let addr = format!("0.0.0.0:{}", entry.remote_port);
            match TcpListener::bind(&addr).await {
                Ok(listener) => {
                    let cancel_token = CancellationToken::new();
                    let listener = Arc::new(listener);

                    // Spawn visitor accept loop for this port
                    let handle = spawn_visitor_accept_loop(
                        entry.remote_port,
                        listener.clone(),
                        registry.clone(),
                        cancel_token.clone(),
                        idle_timeout_secs,
                    );

                    reg.ports.insert(entry.remote_port, PortState {
                        listener,
                        listener_handle: handle,
                        cancel_token,
                        active_routes: Vec::new(),
                    });
                    tracing::info!("Bound tunnel port: 0.0.0.0:{}", entry.remote_port);
                }
                Err(e) => {
                    results.push(TunnelResult {
                        success: false,
                        remote_port: entry.remote_port,
                        sni: entry.sni.clone(),
                        error: Some(format!("bind failed: {}", e)),
                    });
                    continue;
                }
            }
        }

        reg.register_route(&req.client_id, entry);
        results.push(TunnelResult {
            success: true,
            remote_port: entry.remote_port,
            sni: entry.sni.clone(),
            error: None,
        });
    }

    // Add connection to client pool
    reg.add_connection(&req.client_id, ClientConnection {
        handler,
        conn_id,
    });

    // TODO(metrics): increment tunnel_register_total{client_id, result="success/failure"}
    // TODO(metrics): set tunnel_active_routes gauge
    // TODO(metrics): set tunnel_active_connections gauge for this client_id

    results
}

/// Spawn the visitor accept loop for a given port.
fn spawn_visitor_accept_loop(
    port: u16,
    listener: Arc<TcpListener>,
    registry: SharedRegistry,
    cancel_token: CancellationToken,
    idle_timeout_secs: usize,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("Visitor accept loop for port {} cancelled", port);
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            tracing::debug!("Visitor connection from {} on port {}", addr, port);
                            let registry = registry.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_visitor(port, stream, registry, idle_timeout_secs).await {
                                    tracing::warn!("Visitor handler error on port {}: {}", port, e);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Accept error on port {}: {}", port, e);
                            break;
                        }
                    }
                }
            }
        }
    })
}

/// Handle a single visitor connection: SNI route → find tunnel → open reverse stream → relay.
async fn handle_visitor(
    port: u16,
    visitor_stream: tokio::net::TcpStream,
    registry: SharedRegistry,
    idle_timeout_secs: usize,
) -> Result<()> {
    // Step 1: Peek SNI (non-consuming)
    let sni = match peek_sni_v2(&visitor_stream).await {
        Ok(sni) => Some(sni),
        Err(_) => None, // Not TLS or couldn't parse — use default route
    };

    // Step 2: Lookup route and get a connection from client pool (round-robin).
    // IMPORTANT: clone the handler reference and drop the lock BEFORE any async I/O.
    // Holding the Mutex across an await point blocks all other registry operations.
    let (local_addr, handler) = {
        let mut reg = registry.lock().await;
        let tunnel = reg.lookup_route(port, sni.as_deref())
            .ok_or_else(|| anyhow::anyhow!("no route for port {}:{}", port, sni.as_deref().unwrap_or("default")))?;

        let local_addr = tunnel.local_addr.clone();
        let client_id = tunnel.client_id.clone();

        let client_state = reg.clients.get_mut(&client_id)
            .ok_or_else(|| anyhow::anyhow!("client '{}' not found in registry", client_id))?;

        // Use next_connection() which properly advances the round-robin cursor
        let handler = client_state.next_connection()
            .ok_or_else(|| anyhow::anyhow!("client '{}' has no active connections", client_id))?
            .clone(); // Clone the handler out of the lock

        (local_addr, handler)
    }; // Lock dropped here, before any await

    // TODO(metrics): increment tunnel_visitor_total{remote_port, sni}
    // TODO(metrics): increment tunnel_visitor_connections{remote_port, sni}

    // Step 3: Open reverse stream on the mux connection (outside lock)
    match handler {
        ConnectionHandler::Tls(mux_conn) => {
            let mut mux_stream = mux_conn.open_stream().await?;

            // Write FLAG_REVERSE_OPEN + OpenStreamEvent as first frame
            let open_ev = OpenStreamEvent {
                proto: "tcp".to_string(),
                addr: local_addr,
            };
            let ev = event::new_reverse_open_stream_event(0, &open_ev)?;
            let (mut stream_r, mut stream_w) = tokio::io::split(mux_stream);
            event::write_event(&mut stream_w, ev).await?;

            // Bidirectional relay between visitor and mux stream
            let (mut visitor_r, mut visitor_w) = tokio::io::split(visitor_stream);
            let mut relay = Stream::new(&mut visitor_r, &mut visitor_w, &mut stream_r, &mut stream_w);
            relay.transfer(idle_timeout_secs).await?;
        }
        #[cfg(feature = "s2n_quic")]
        ConnectionHandler::Quic(handle) => {
            let mut handle = handle.clone();

            let stream = handle.open_bidirectional_stream().await
                .map_err(|e| anyhow::anyhow!("QUIC open_bidirectional_stream failed: {}", e))?;
            let (mut recv_stream, mut send_stream) = stream.split();

            let open_ev = OpenStreamEvent {
                proto: "tcp".to_string(),
                addr: local_addr,
            };
            let ev = event::new_reverse_open_stream_event(0, &open_ev)?;
            event::write_event(&mut send_stream, ev).await?;

            let (mut visitor_r, mut visitor_w) = tokio::io::split(visitor_stream);
            let mut relay = Stream::new(&mut visitor_r, &mut visitor_w, &mut recv_stream, &mut send_stream);
            relay.transfer(idle_timeout_secs).await?;
        }
    }

    Ok(())
}
```

- [ ] **Step 2: Register module in tunnel/mod.rs**

```rust
pub mod tunnel_remote;
```

- [ ] **Step 3: Verify peek_sni_v2 is public**

`peek_sni_v2` in `src/tunnel/tls_local.rs` is already `pub` — no change needed.
Just verify it's accessible: `grep "pub async fn peek_sni_v2" src/tunnel/tls_local.rs`

- [ ] **Step 4: Verify compilation**

Run: `cargo build 2>&1 | head -30`
Expected: Builds successfully

- [ ] **Step 5: Commit**

```bash
git add src/tunnel/tunnel_remote.rs src/tunnel/mod.rs src/tunnel/tls_local.rs
git commit -m "feat(tunnel): implement visitor accept loop with SNI routing and reverse stream relay"
```

---

## Task 8: Wire Up Server — Pass Registry to TLS Server

**Files:**
- Modify: `src/main.rs`
- Modify: `src/tunnel/tls_remote.rs` (update `start_tls_remote_server` signature)

- [ ] **Step 1: Create registry in main.rs server branch and pass to TLS server**

In `service_main()`, inside `Role::Server`, before starting the server:

```rust
        Role::Server => {
            let registry = if let Some(ranges) = tunnel_port_ranges {
                let listen_port = args.listen.port();
                let admin_port = args.admin_listen.port();
                let reserved = vec![listen_port, admin_port];
                Some(Arc::new(tokio::sync::Mutex::new(
                    tunnel::tunnel_registry::TunnelRegistry::new(ranges, reserved)
                )))
            } else {
                None
            };

            match args.protocol {
                Protocol::Tls => {
                    tunnel::start_tls_remote_server(
                        &args.listen,
                        &args.cert,
                        &args.key,
                        args.idle_timeout_secs,
                        args.mux_stream_channel_size,
                        registry,
                    ).await?;
                }
                // ... QUIC branch similar
            }
        }
```

- [ ] **Step 2: Update start_tls_remote_server to accept and pass registry**

In `tls_remote.rs`, update the function signature and the spawn inside the accept loop:

```rust
pub async fn start_tls_remote_server(
    listen: &SocketAddr,
    cert_path: &Path,
    key_path: &Path,
    idle_timeout_secs: usize,
    stream_channel_size: usize,
    registry: Option<crate::tunnel::tunnel_registry::SharedRegistry>,
) -> Result<()> {
    // ... existing setup ...
    loop {
        let (stream, _) = listener.accept().await?;
        let registry = registry.clone();
        // ... existing id logic ...
        let fut = async move {
            let stream = acceptor.accept(stream).await?;
            handle_tls_connection(stream, conn_id, idle_timeout_secs, stream_channel_size, registry).await?;
            Ok(()) as Result<()>
        };
        // ... existing spawn ...
    }
}
```

- [ ] **Step 3: Add necessary imports to main.rs**

```rust
use std::sync::Arc;
```

- [ ] **Step 4: Build and verify E2E path compiles**

Run: `cargo build 2>&1 | head -20`
Expected: Successful build

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/tunnel/tls_remote.rs
git commit -m "feat(tunnel): wire TunnelRegistry into TLS server startup"
```

---

## Task 9: QUIC Mode — Server and Client Tunnel Support

**Files:**
- Modify: `src/tunnel/s2n_quic_client.rs`
- Modify: `src/tunnel/s2n_quic_remote.rs`

- [ ] **Step 1: Make s2n_quic helper functions accessible and implement QUIC tunnel client**

In `src/tunnel/s2n_quic_client.rs`, first change the visibility of helper functions:

```rust
// Change: fn new_s2n_quic_endpoint → pub(crate) fn new_s2n_quic_endpoint
pub(crate) fn new_s2n_quic_endpoint(_url: &Url, cert_path: &Path) -> anyhow::Result<s2n_quic::client::Client> {

// Change: async fn new_s2n_quic_connection → pub(crate) async fn new_s2n_quic_connection
pub(crate) async fn new_s2n_quic_connection(
```

Then add the QUIC tunnel client function:

```rust
/// Tunnel client loop for QUIC mode using Connection::split()
pub async fn start_tunnel_client_quic(
    url: &Url,
    cert_path: &Path,
    host: &str,
    client_id: &str,
    entries: Vec<TunnelEntry>,
    idle_timeout_secs: usize,
) -> anyhow::Result<()> {
    loop {
        match run_quic_tunnel_connection(url, cert_path, host, client_id, &entries, idle_timeout_secs).await {
            Ok(()) => tracing::info!("QUIC tunnel connection closed, reconnecting..."),
            Err(e) => tracing::error!("QUIC tunnel error: {}, reconnecting...", e),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn run_quic_tunnel_connection(
    url: &Url,
    cert_path: &Path,
    host: &str,
    client_id: &str,
    entries: &[TunnelEntry],
    idle_timeout_secs: usize,
) -> anyhow::Result<()> {
    let endpoint = new_s2n_quic_endpoint(url, cert_path)?;
    let mut connection = new_s2n_quic_connection(&endpoint, url, host).await?;

    // Phase 1: Auth on full connection
    let auth_stream = connection.open_bidirectional_stream().await
        .map_err(|e| anyhow!("open auth stream: {}", e))?;
    let (mut recv, mut send) = auth_stream.split();

    let auth_req = AuthRequest::Register(RegisterRequest {
        client_id: client_id.to_string(),
        tunnels: entries.to_vec(),
    });
    let ev = event::new_auth_event(0, &auth_req)?;
    event::write_event(&mut send, ev).await?;

    let ack_ev = event::read_event(&mut recv).await?;
    if ack_ev.header.flags() != FLAG_AUTH_ACK {
        return Err(anyhow!("expected FLAG_AUTH_ACK, got flag={}", ack_ev.header.flags()));
    }
    let config = bincode::config::standard();
    let (ack, _): (AuthAck, usize) = bincode::decode_from_slice(ack_ev.body.as_ref(), config)
        .map_err(|e| anyhow!("decode AuthAck failed: {}", e))?;
    match ack {
        AuthAck::Proxy => return Err(anyhow!("server returned Proxy ack for tunnel request")),
        AuthAck::RegisterAck(register_ack) => {
            crate::tunnel::tunnel_client::handle_register_ack(&register_ack)?;
        }
    }
    drop(send);
    drop(recv);

    // Phase 2: Split and accept reverse streams
    let (_handle, mut acceptor) = connection.split();

    while let Ok(Some(stream)) = acceptor.accept_bidirectional_stream().await {
        let (mut recv_stream, mut send_stream) = stream.split();
        tokio::spawn(async move {
            if let Err(e) = crate::tunnel::tunnel_client::handle_reverse_stream(&mut recv_stream, &mut send_stream, idle_timeout_secs).await {
                tracing::warn!("QUIC reverse stream error: {}", e);
            }
        });
    }
    Ok(())
}
```

- [ ] **Step 2: Update QUIC server to support auth dispatch**

In `src/tunnel/s2n_quic_remote.rs`, refactor `start_quic_remote_server`:

```rust
pub async fn start_quic_remote_server(
    listen: &SocketAddr,
    cert_path: &Path,
    key_path: &Path,
    idle_timeout_secs: usize,
    registry: Option<crate::tunnel::tunnel_registry::SharedRegistry>,
) -> Result<()> {
    // ... existing server setup ...
    while let Some(mut connection) = server.accept().await {
        let registry = registry.clone();
        tokio::spawn(async move {
            // Auth stream: first bidirectional stream
            let Ok(Some(auth_stream)) = connection.accept_bidirectional_stream().await else {
                return;
            };
            let (mut recv, mut send) = auth_stream.split();

            let Ok(ev) = event::read_event(&mut recv).await else { return; };
            if ev.header.flags() != event::FLAG_AUTH { return; }

            let config = bincode::config::standard();
            let Ok((auth_req, _)): Result<(event::AuthRequest, usize), _> =
                bincode::decode_from_slice(ev.body.as_ref(), config) else { return; };

            match auth_req {
                event::AuthRequest::Proxy => {
                    // Proxy mode: ack then accept loop
                    let ack = event::AuthAck::Proxy;
                    let _ = event::write_event(&mut send, event::new_auth_ack_event(0, &ack).unwrap()).await;
                    drop(recv); drop(send);

                    while let Ok(Some(stream)) = connection.accept_bidirectional_stream().await {
                        let (mut r, mut s) = stream.split();
                        tokio::spawn(async move {
                            let _ = handle_server_stream(&mut r, &mut s, idle_timeout_secs).await;
                        });
                    }
                }
                event::AuthRequest::Register(register_req) => {
                    // Tunnel mode: register, split, store handle
                    let results = if let Some(registry) = registry {
                        // Auth complete — split connection
                        let (handle, mut acceptor) = connection.split();

                        // Register via shared handler (same as TLS path)
                        let results = crate::tunnel::tunnel_remote::handle_tunnel_register(
                            &registry,
                            &register_req,
                            crate::tunnel::tunnel_registry::ConnectionHandler::Quic(handle.clone()),
                            0, // conn_id not tracked for QUIC
                            idle_timeout_secs,
                        ).await;

                        // Spawn acceptor drain task to detect disconnect
                        let client_id = register_req.client_id.clone();
                        let registry_clone = registry.clone();
                        tokio::spawn(async move {
                            // Accept streams until connection closes
                            while acceptor.accept_bidirectional_stream().await.is_ok_and(|v| v.is_some()) {}
                            // Connection lost — cleanup
                            tracing::info!("QUIC tunnel client '{}' disconnected", client_id);
                            let mut reg = registry_clone.lock().await;
                            let no_connections = reg.remove_connection(&client_id, 0);
                            if no_connections {
                                let empty_ports = reg.remove_client_routes(&client_id);
                                for port in empty_ports {
                                    if let Some(port_state) = reg.ports.remove(&port) {
                                        port_state.cancel_token.cancel();
                                    }
                                }
                            }
                        });

                        results
                    } else {
                        // Tunnel not enabled
                        register_req.tunnels.iter().map(|t| event::TunnelResult {
                            success: false,
                            remote_port: t.remote_port,
                            sni: t.sni.clone(),
                            error: Some("tunnel not enabled on server".to_string()),
                        }).collect()
                    };

                    let ack = event::AuthAck::RegisterAck(event::RegisterAck { results });
                    let _ = event::write_event(&mut send, event::new_auth_ack_event(0, &ack).unwrap()).await;
                    drop(recv); drop(send);
                }
            }
        });
    }
    Ok(())
}
```

- [ ] **Step 3: Update tunnel client dispatcher for QUIC**

In the `start_tunnel_client` dispatcher (from Task 5), replace the QUIC stub:

```rust
        #[cfg(feature = "s2n_quic")]
        "quic" => {
            s2n_quic_client::start_tunnel_client_quic(
                url, cert_path, host, client_id, entries, idle_timeout_secs,
            ).await
        }
```

- [ ] **Step 4: Verify compilation with s2n_quic feature**

Run: `cargo build --features s2n_quic 2>&1 | head -30`
Expected: Builds (or minor fixups)

- [ ] **Step 5: Commit**

```bash
git add src/tunnel/s2n_quic_client.rs src/tunnel/s2n_quic_remote.rs src/tunnel/tunnel_client.rs
git commit -m "feat(tunnel): add QUIC mode support for tunnel client and server"
```

---

## Task 10: Integration — Proxy Mode Auth Stream (Client Side)

> **E2E Dependency Note:** Task 6 (server-side auth dispatch for proxy) and Task 10
> (client-side proxy auth) MUST both be complete before proxy mode can be tested end-to-end.
> After Task 6, new servers require auth but old clients don't send it. After Task 10,
> new clients send auth but old servers don't expect it. There is no backwards-compatible
> intermediate state — both sides must be upgraded together.

**Files:**
- Modify: `src/tunnel/client.rs` — add auth stream send before proxy data streams
- Modify: `src/tunnel/tls_client.rs` — modify connection establishment to send auth

- [ ] **Step 1: Add auth stream handshake in TLS MuxClient::from()**

In `src/tunnel/tls_client.rs`, inside `MuxClient::<TlsConnection>::from()` (line ~81),
after `tls_conn.connect()` succeeds and before `client.conns.push(tls_conn)`, add auth stream:

```rust
    // Inside the for loop in MuxClient::<TlsConnection>::from():
    // After: match tls_conn.connect(url, cert_path, host).await { Err(e) => ... _ => { ... } }

    // Send auth stream for proxy mode (before adding to pool)
    if let Some(ref mut conn) = tls_conn.inner {
        let auth_stream = conn.open_stream().await?;
        let (mut auth_r, mut auth_w) = tokio::io::split(auth_stream);
        let auth_req = event::AuthRequest::Proxy;
        let ev = event::new_auth_event(0, &auth_req)?;
        event::write_event(&mut auth_w, ev).await?;

        let ack_ev = event::read_event(&mut auth_r).await?;
        if ack_ev.header.flags() != event::FLAG_AUTH_ACK {
            return Err(anyhow!("proxy auth failed: unexpected flag {}", ack_ev.header.flags()));
        }
        // Auth stream done — halves dropped when they go out of scope
        tracing::info!("TLS connection:{} auth completed (proxy mode)", i);
    }
```

> **Why `conn.open_stream()` instead of `tls_conn.open_stream()`?** Because
> `TlsConnection::open_stream()` returns `(WriteHalf, ReadHalf)` whereas we need
> the raw `MuxStream` to split ourselves. Accessing `tls_conn.inner` directly
> gives us the `mux::Connection` whose `open_stream()` returns `MuxStream`.

- [ ] **Step 2: Add auth stream handshake in QUIC MuxClient::from()**

In `src/tunnel/s2n_quic_client.rs`, inside `MuxClient::<S2NQuicConnection>::from()` (line ~77),
after `quic_conn.connect()` succeeds and before `client.conns.push(quic_conn)`, add auth stream:

```rust
    // Inside the for loop in MuxClient::<S2NQuicConnection>::from():
    // After: match quic_conn.connect(url, cert_path, host).await { ... }

    // Send auth stream for proxy mode (before adding to pool)
    if let Some(ref mut connection) = quic_conn.inner {
        let auth_stream = connection.open_bidirectional_stream().await
            .map_err(|e| anyhow!("open auth stream: {}", e))?;
        let (mut auth_r, mut auth_w) = auth_stream.split();
        let auth_req = event::AuthRequest::Proxy;
        let ev = event::new_auth_event(0, &auth_req)?;
        event::write_event(&mut auth_w, ev).await?;

        let ack_ev = event::read_event(&mut auth_r).await?;
        if ack_ev.header.flags() != event::FLAG_AUTH_ACK {
            return Err(anyhow!("proxy auth failed: unexpected flag {}", ack_ev.header.flags()));
        }
        tracing::info!("QUIC connection:{} auth completed (proxy mode)", i);
    }
```

- [ ] **Step 3: Verify existing proxy mode still works**

Run: `cargo build && cargo test 2>&1 | tail -20`
Expected: Builds, existing tests pass

- [ ] **Step 4: Commit**

```bash
git add src/tunnel/tls_client.rs src/tunnel/s2n_quic_client.rs
git commit -m "feat(tunnel): add proxy mode auth stream handshake on client connection setup"
```

---

## Task 11: End-to-End Smoke Test

**Files:**
- Manual testing steps (no new files)

- [ ] **Step 1: Build release**

```bash
cargo build --release
```

- [ ] **Step 2: Generate test certificates**

```bash
./target/release/rsnova --rcgen --tls_host localhost
```

- [ ] **Step 3: Start server with tunnel port range**

```bash
./target/release/rsnova --role server --protocol tls --listen 127.0.0.1:48100 \
  --key key.pem --cert cert.pem --tunnel-port-range 8000-9000 &
```

- [ ] **Step 4: Start a local echo service on port 9999**

```bash
# Simple TCP echo server for testing
nc -l -k 9999 &
```

- [ ] **Step 5: Start tunnel client**

```bash
./target/release/rsnova --role client --remote tls://127.0.0.1:48100 \
  --cert cert.pem --tls_host localhost \
  --tunnel-client-id test --tunnel 9999:8080
```

Expected: Log shows "Tunnel registered: :8080 → OK"

- [ ] **Step 6: Test reverse connection**

```bash
echo "hello" | nc 127.0.0.1 8080
```

Expected: "hello" appears on the nc echo server; response flows back.

- [ ] **Step 7: Test proxy mode still works**

Start a separate proxy client (without --tunnel) and verify HTTP proxy works as before.

- [ ] **Step 8: Commit any fixes**

```bash
git add -A
git commit -m "fix: address issues found during E2E smoke testing"
```

---

## Notes

- **Concurrent connections:** The initial implementation uses a single connection per tunnel client. Multi-connection (`--concurrent=N`) support follows the same pattern — each connection independently does auth + enters the pool. This can be a follow-up enhancement.
- **Metrics:** The spec defines metrics (`tunnel_active_routes`, `tunnel_visitor_connections`, etc.). Placeholder TODO comments are included in the code; these should be implemented incrementally after the core path works.
- **Reconnection:** The basic reconnection loop is in `start_tunnel_client_tls`. Production-grade exponential backoff is a follow-up.
- **Round-robin cursor:** The cursor is properly advanced via `ClientState::next_connection()` inside the registry lock, and the handler is cloned out of the lock before any async I/O. This ensures both correctness and no lock contention.
