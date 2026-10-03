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
    /// Draining handlers stay registered until their transport closes so
    /// routes and listeners remain alive, but are no longer selected for new
    /// visitors.
    pub draining: bool,
    /// Consecutive `open_stream` failures observed while serving visitors.
    /// Reset on any success. When it reaches `MAX_HANDLER_FAILURES` the
    /// connection is considered dead and `next_connection` drops it so
    /// visitors stop being routed to a black hole.
    pub consecutive_failures: u32,
    /// Slot-index identity of the connection's handler within the client's
    /// pool. Unlike `conn_id` (which is assigned by the remote and may repeat
    /// across reconnects), this is unique per pool slot for the lifetime of
    /// the registry entry, letting `record_open_result` find the right
    /// `ClientConnection` after `next_connection` has cloned the handler out.
    pub slot: usize,
}

/// Number of consecutive `open_stream` failures after which a handler is
/// considered dead and removed from the client's pool.
pub const MAX_HANDLER_FAILURES: u32 = 3;

pub struct ClientState {
    pub connections: Vec<ClientConnection>,
    pub routes: Vec<RouteKey>,
    pub cursor: usize,
    /// The tunnel entries from the most recent successful registration,
    /// deduplicated. `handle_tls_connection` replays these into
    /// `register_route` when a visitor finds no route, covering the window
    /// where a second connection's disconnect briefly wiped the routes of a
    /// still-live first connection (the "tunnel black hole" race).
    pub last_entries: Vec<crate::mux::event::TunnelEntry>,
}

impl ClientState {
    /// Round-robin select a connection from the pool.
    ///
    /// Skips connections whose handler has accumulated `MAX_HANDLER_FAILURES`
    /// consecutive `open_stream` failures (a dead mux connection that was not
    /// yet torn down by its accept loop — e.g. an idle tunnel control
    /// connection whose NAT session silently died). The stale entries are
    /// pruned here so a half-open pool stops handing visitors to a black
    /// hole. Returns the handler plus its pool slot so `record_open_result`
    /// can attribute the outcome back to the right entry.
    pub fn next_connection(&mut self) -> Option<(ConnectionHandler, usize)> {
        if self.connections.is_empty() {
            return None;
        }
        // Prune connections that repeatedly failed to open streams. One
        // visitor failure already makes the handler suspect; three in a row
        // (with no success in between, since a success resets the counter)
        // is strong evidence the underlying connection is gone.
        let before = self.connections.len();
        self.connections
            .retain(|c| c.consecutive_failures < MAX_HANDLER_FAILURES);
        if self.connections.len() != before {
            tracing::warn!(
                "pruned {} dead tunnel connection(s) from client pool ({} left)",
                before - self.connections.len(),
                self.connections.len()
            );
        }
        if self.connections.is_empty() {
            return None;
        }
        for _ in 0..self.connections.len() {
            let idx = self.cursor % self.connections.len();
            self.cursor = self.cursor.wrapping_add(1);
            let conn = &self.connections[idx];
            if !conn.draining {
                return Some((conn.handler.clone(), conn.slot));
            }
        }
        None
    }

    /// Record the outcome of an `open_stream` attempt against the handler in
    /// pool slot `slot`. Success resets the failure counter; failure
    /// increments it.
    pub fn record_open_result(&mut self, slot: usize, success: bool) {
        if let Some(conn) = self.connections.iter_mut().find(|c| c.slot == slot) {
            if success {
                conn.consecutive_failures = 0;
            } else {
                conn.consecutive_failures = conn.consecutive_failures.saturating_add(1);
            }
        }
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
                last_entries: Vec::new(),
            });
        if !client_state.routes.contains(&key) {
            client_state.routes.push(key.clone());
        }
        if !client_state.last_entries.contains(entry) {
            client_state.last_entries.push(entry.clone());
        }
        if let Some(port_state) = self.ports.get_mut(&entry.remote_port)
            && !port_state.active_routes.contains(&key)
        {
            port_state.active_routes.push(key);
        }
    }

    /// Make the client's route set match a successfully registered generation.
    /// This is called only after at least one replacement route succeeds, so a
    /// completely failed replacement cannot erase the still-serving old set.
    pub fn reconcile_client_routes(
        &mut self,
        client_id: &str,
        entries: &[TunnelEntry],
    ) -> Vec<u16> {
        let desired: Vec<RouteKey> = entries
            .iter()
            .map(|entry| RouteKey {
                remote_port: entry.remote_port,
                sni: entry.sni.clone(),
            })
            .collect();
        let obsolete = self
            .clients
            .get(client_id)
            .map(|client| {
                client
                    .routes
                    .iter()
                    .filter(|route| !desired.contains(route))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let mut empty_ports = Vec::new();
        for route in &obsolete {
            self.routes.remove(route);
            if let Some(port_state) = self.ports.get_mut(&route.remote_port) {
                port_state.active_routes.retain(|active| active != route);
                if port_state.active_routes.is_empty() {
                    empty_ports.push(route.remote_port);
                }
            }
        }
        if let Some(client) = self.clients.get_mut(client_id) {
            client.routes.retain(|route| desired.contains(route));
            client.last_entries = entries.to_vec();
        }
        empty_ports
    }

    /// Remove all routes for a client. Returns ports that have no remaining routes (should be unbound).
    ///
    /// The client entry itself (and its `last_entries`) is intentionally kept
    /// so that `handle_visitor` can re-register the wiped routes if another
    /// connection for the same client is still alive — the self-healing path
    /// for the "one connection's disconnect wiped a live connection's routes"
    /// race. `ClientState` is tiny (a few Vec headers), and the next
    /// `register_route`/`add_connection` for this client reuses the entry.
    pub fn remove_client_routes(&mut self, client_id: &str) -> Vec<u16> {
        let mut empty_ports = Vec::new();
        if let Some(client_state) = self.clients.get_mut(client_id) {
            let routes_to_remove: Vec<RouteKey> = std::mem::take(&mut client_state.routes);
            client_state.connections.clear();
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

    /// Stop selecting one connection for new visitors while retaining it in
    /// the registry until transport teardown finishes its active streams.
    pub fn begin_drain(&mut self, client_id: &str, conn_id: u32) -> bool {
        let Some(client_state) = self.clients.get_mut(client_id) else {
            return false;
        };
        let Some(conn) = client_state
            .connections
            .iter_mut()
            .find(|conn| conn.conn_id == conn_id)
        else {
            return false;
        };
        conn.draining = true;
        true
    }

    /// Add a connection to a client's pool. The new connection is assigned
    /// the next pool slot (monotonic, never reused while the client exists).
    pub fn add_connection(&mut self, client_id: &str, mut conn: ClientConnection) {
        let client_state = self
            .clients
            .entry(client_id.to_string())
            .or_insert_with(|| ClientState {
                connections: Vec::new(),
                routes: Vec::new(),
                cursor: 0,
                last_entries: Vec::new(),
            });
        let slot = client_state
            .connections
            .iter()
            .map(|c| c.slot + 1)
            .max()
            .unwrap_or(0);
        conn.slot = slot;
        client_state.connections.push(conn);
    }

    /// Record whether a reverse-stream open succeeded against the handler in
    /// pool slot `slot`. Feeds the consecutive-failure counter used by
    /// `next_connection` to prune dead handlers.
    pub fn record_open_result(&mut self, client_id: &str, slot: usize, success: bool) {
        if let Some(client_state) = self.clients.get_mut(client_id) {
            client_state.record_open_result(slot, success);
        }
    }

    /// Snapshot the last registered tunnel entries for a client, for
    /// self-healing re-registration after a transient route wipe.
    pub fn last_entries_of(&self, client_id: &str) -> Vec<crate::mux::event::TunnelEntry> {
        self.clients
            .get(client_id)
            .map(|c| c.last_entries.clone())
            .unwrap_or_default()
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
            if let Some(t) = self
                .routes
                .values()
                .find(|t| t.remote_port == remote_port && t.sni.as_deref() == Some(sni_str))
            {
                return Some(t);
            }
        }
        // Fall back to (port, None).
        self.routes
            .values()
            .find(|t| t.remote_port == remote_port && t.sni.is_none())
    }

    pub fn port_has_sni_routes(&self, remote_port: u16) -> bool {
        self.routes
            .keys()
            .any(|route| route.remote_port == remote_port && route.sni.is_some())
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

    #[test]
    fn port_sni_inspection_only_enabled_when_needed() {
        let mut reg = TunnelRegistry::new(vec![(8000, 9000)], vec![]);
        reg.register_route("client-a", &entry(8080, None));
        assert!(!reg.port_has_sni_routes(8080));

        reg.register_route("client-a", &entry(8080, Some("api.example.com")));
        assert!(reg.port_has_sni_routes(8080));
        assert!(!reg.port_has_sni_routes(8081));
    }

    #[test]
    fn replacement_registration_removes_only_obsolete_routes() {
        let mut reg = TunnelRegistry::new(vec![(8000, 9000)], vec![]);
        let old = entry(8080, None);
        let kept = entry(8081, None);
        reg.register_route("client-a", &old);
        reg.register_route("client-a", &kept);

        reg.reconcile_client_routes("client-a", std::slice::from_ref(&kept));

        assert!(reg.lookup_route(8080, None).is_none());
        assert!(reg.lookup_route(8081, None).is_some());
        assert_eq!(reg.last_entries_of("client-a"), vec![kept]);
    }

    /// Regression for the "tunnel black hole" race: when a client's routes
    /// are wiped (e.g. one of its connections disconnected) but it still has
    /// a live connection, `last_entries_of` must remember the entries so
    /// `handle_visitor` can re-register them on the spot.
    #[test]
    fn last_entries_survive_route_wipe() {
        let mut reg = TunnelRegistry::new(vec![(8000, 9000)], vec![]);
        let e = entry(8080, None);
        reg.register_route("client-a", &e);

        // Simulate the wipe caused by a disconnecting second connection.
        let _empty_ports = reg.remove_client_routes("client-a");
        assert!(reg.lookup_route(8080, None).is_none());

        // last_entries must still hold the entry for self-healing.
        let remembered = reg.last_entries_of("client-a");
        assert!(
            remembered.iter().any(|t| t.remote_port == 8080),
            "last_entries must remember the wiped route"
        );
    }

    /// Build a `ConnectionHandler::Tls` backed by a real mux::Connection over
    /// a never-serviced duplex pair — the handler is only used as a pool
    /// placeholder in these tests, never opened.
    fn dummy_tls_handler() -> (ConnectionHandler, tokio::io::DuplexStream) {
        let (a, b) = tokio::io::duplex(1024);
        let (a_r, a_w) = tokio::io::split(a);
        let conn = crate::mux::Connection::new_with_stream_window(
            a_r,
            a_w,
            crate::mux::Mode::Client,
            0,
            crate::mux::INITIAL_STREAM_WINDOW,
        );
        (ConnectionHandler::Tls(std::sync::Arc::new(conn)), b)
    }

    /// `next_connection` must prune handlers that repeatedly failed
    /// `open_stream`, so visitors stop being routed to a dead mux connection
    /// whose accept-loop teardown hasn't fired yet.
    #[tokio::test]
    async fn dead_handler_pruned_after_repeated_open_failures() {
        let mut reg = TunnelRegistry::new(vec![(8000, 9000)], vec![]);
        // Two connections for the same client; keep the duplex peers alive so
        // the mux dispatchers don't immediately exit.
        let (h1, _p1) = dummy_tls_handler();
        let (h2, _p2) = dummy_tls_handler();
        for (conn_id, handler) in [(1u32, h1), (2u32, h2)] {
            reg.add_connection(
                "client-a",
                ClientConnection {
                    handler,
                    conn_id,
                    draining: false,
                    consecutive_failures: 0,
                    slot: 0,
                },
            );
        }
        assert_eq!(reg.clients["client-a"].connections.len(), 2);
        // add_connection assigns monotonically increasing slots.
        assert_eq!(reg.clients["client-a"].connections[0].slot, 0);
        assert_eq!(reg.clients["client-a"].connections[1].slot, 1);

        // Fail slot 0 up to the threshold. Round-robin cursor starts at 0,
        // so the first pick is slot 0.
        for _ in 0..MAX_HANDLER_FAILURES {
            reg.record_open_result("client-a", 0, false);
        }

        // Next pick must prune slot 0 and hand out slot 1.
        let (_handler, slot) = reg
            .clients
            .get_mut("client-a")
            .unwrap()
            .next_connection()
            .expect("a healthy handler must remain");
        assert_eq!(slot, 1, "dead slot 0 must be pruned, slot 1 survives");
        assert_eq!(reg.clients["client-a"].connections.len(), 1);
        assert_eq!(reg.clients["client-a"].connections[0].conn_id, 2);
    }

    /// A successful `open_stream` resets the consecutive-failure counter, so
    /// one transient failure doesn't mark a healthy handler for pruning.
    #[tokio::test]
    async fn success_resets_failure_counter() {
        let mut reg = TunnelRegistry::new(vec![(8000, 9000)], vec![]);
        let (handler, _peer) = dummy_tls_handler();
        reg.add_connection(
            "client-a",
            ClientConnection {
                handler,
                conn_id: 7,
                draining: false,
                consecutive_failures: 0,
                slot: 0,
            },
        );
        reg.record_open_result("client-a", 0, false);
        assert_eq!(
            reg.clients["client-a"].connections[0].consecutive_failures,
            1
        );
        reg.record_open_result("client-a", 0, true);
        assert_eq!(
            reg.clients["client-a"].connections[0].consecutive_failures,
            0
        );
    }

    #[tokio::test]
    async fn begin_drain_stops_selection_but_preserves_routes_and_peers() {
        let mut reg = TunnelRegistry::new(vec![(8000, 9000)], vec![]);
        let route = entry(8080, None);
        reg.register_route("client-a", &route);
        let (old_handler, _old_peer) = dummy_tls_handler();
        let (new_handler, _new_peer) = dummy_tls_handler();
        for (conn_id, handler) in [(1, old_handler), (2, new_handler)] {
            reg.add_connection(
                "client-a",
                ClientConnection {
                    handler,
                    conn_id,
                    draining: false,
                    consecutive_failures: 0,
                    slot: 0,
                },
            );
        }

        assert!(reg.begin_drain("client-a", 1));
        assert_eq!(reg.clients["client-a"].connections.len(), 2);
        assert!(reg.lookup_route(8080, None).is_some());

        for _ in 0..4 {
            let (_, slot) = reg
                .clients
                .get_mut("client-a")
                .unwrap()
                .next_connection()
                .unwrap();
            assert_eq!(slot, 1, "draining slot must never serve a new visitor");
        }

        assert!(!reg.remove_connection("client-a", 1));
        assert_eq!(reg.clients["client-a"].connections.len(), 1);
        assert!(reg.lookup_route(8080, None).is_some());
    }
}
