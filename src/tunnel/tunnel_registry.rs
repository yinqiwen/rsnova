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

#[allow(dead_code)]
pub struct ActiveTunnel {
    pub client_id: String,
    pub local_addr: String,
    pub remote_port: u16,
    pub sni: Option<String>,
}

#[allow(dead_code)]
pub struct PortState {
    pub listener: Arc<TcpListener>,
    pub listener_handle: JoinHandle<()>,
    pub cancel_token: CancellationToken,
    pub active_routes: Vec<RouteKey>,
}

/// Abstraction over TLS mux::Connection and QUIC Handle for opening reverse streams.
#[derive(Clone)]
pub enum ConnectionHandler {
    Tls(Arc<crate::mux::Connection>),
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
    pub fn validate_entry(&self, client_id: &str, entry: &TunnelEntry) -> Option<String> {
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
        if let Some(existing) = self.routes.get(&key)
            && existing.client_id != client_id
        {
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
        let client_state = self
            .clients
            .entry(client_id.to_string())
            .or_insert_with(|| ClientState {
                connections: Vec::new(),
                routes: Vec::new(),
                cursor: 0,
            });
        if !client_state.routes.contains(&key) {
            client_state.routes.push(key.clone());
        }
        if let Some(port_state) = self.ports.get_mut(&entry.remote_port)
            && !port_state.active_routes.contains(&key)
        {
            port_state.active_routes.push(key);
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
        let client_state = self
            .clients
            .entry(client_id.to_string())
            .or_insert_with(|| ClientState {
                connections: Vec::new(),
                routes: Vec::new(),
                cursor: 0,
            });
        client_state.connections.push(conn);
    }

    /// Lookup a route by (remote_port, sni). Falls back to (remote_port, None) if SNI not found.
    ///
    /// We avoid constructing a `RouteKey { sni: Some(sni.to_string()) }` on every
    /// lookup (the previous implementation did, allocating a `String` per
    /// visitor). Instead, we iterate the routes map and compare by value. The
    /// number of routes per port is typically small (<= 10), so O(n) here is
    /// cheaper than the avoided allocation + hash.
    pub fn lookup_route(&self, remote_port: u16, sni: Option<&str>) -> Option<&ActiveTunnel> {
        if let Some(sni_str) = sni {
            // Look for an exact (port, Some(sni)) match first.
            if let Some(t) = self.routes.values().find(|t| {
                t.remote_port == remote_port && t.sni.as_deref() == Some(sni_str)
            }) {
                return Some(t);
            }
        }
        // Fall back to (port, None).
        self.routes.values().find(|t| {
            t.remote_port == remote_port && t.sni.is_none()
        })
    }

    /// Check if a port already has a listener registered.
    pub fn has_port(&self, port: u16) -> bool {
        self.ports.contains_key(&port)
    }
}

pub type SharedRegistry = Arc<Mutex<TunnelRegistry>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(remote_port: u16, sni: Option<&str>) -> TunnelEntry {
        TunnelEntry {
            local_addr: "localhost:80".to_string(),
            remote_port,
            sni: sni.map(str::to_string),
        }
    }

    #[test]
    fn allows_same_client_to_register_same_route_for_multiple_connections() {
        let mut reg = TunnelRegistry::new(vec![(8000, 9000)], vec![]);
        let e = entry(8080, None);
        assert_eq!(reg.validate_entry("client-a", &e), None);
        reg.register_route("client-a", &e);

        assert_eq!(reg.validate_entry("client-a", &e), None);
        assert!(reg.validate_entry("client-b", &e).is_some());
    }
}
