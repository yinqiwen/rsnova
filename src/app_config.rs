use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::mux::event::TunnelEntry;

/// Parameters that can be hot-reloaded via admin /config page (client only).
pub struct ReloadableConfig {
    pub tunnel_entries: Vec<TunnelEntry>,
    pub tunnel_client_id: String,
}

/// Read-only parameters displayed on the config page.
pub struct StaticArgs {
    pub listen: String,
    pub role: String,
    pub is_tunnel: bool,
    pub remote: Option<String>,
    pub key: String,
    pub cert: String,
    pub tls_host: String,
    pub concurrent: usize,
    pub threads: usize,
    pub idle_timeout_secs: usize,
    pub max_connections: usize,
    pub admin_listen: String,
    pub tproxy: bool,
}

/// Shared application configuration passed to admin server and tunnel client.
pub struct AppConfig {
    pub reloadable: Arc<Mutex<ReloadableConfig>>,
    pub static_args: StaticArgs,
    /// CancellationToken used to notify tunnel client to reconnect with new config.
    /// Wrapped in Arc<Mutex<>> so it can be replaced after cancel (CancellationToken is one-shot).
    pub reload_token: Arc<Mutex<CancellationToken>>,
}

impl AppConfig {
    /// Cancel the current reload token and replace it with a fresh one.
    /// The tunnel client should be selecting on a clone of the token;
    /// cancelling wakes it up so it re-reads config and reconnects.
    pub async fn trigger_reload(&self) {
        let mut token = self.reload_token.lock().await;
        token.cancel();
        *token = CancellationToken::new();
    }

    /// Get a clone of the current reload token for use in select!.
    pub async fn reload_token_clone(&self) -> CancellationToken {
        self.reload_token.lock().await.clone()
    }
}
