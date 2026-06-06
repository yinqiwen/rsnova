mod client;
mod http_local;
mod local;
#[cfg(target_os = "linux")]
mod udp_local;

mod s2n_quic_client;
mod s2n_quic_remote;
mod socks5_local;
mod stream;
mod tls_client;
mod tls_local;
mod tls_remote;

mod transparent;

pub mod tunnel_client;
pub mod tunnel_config;
pub mod tunnel_registry;
pub mod tunnel_remote;

pub use self::s2n_quic_client::new_quic_client;
pub use self::s2n_quic_client::start_tunnel_client_quic;
pub use self::s2n_quic_remote::start_quic_remote_server;

// pub const DEFAULT_TLS_HOST: &str = "google.com";
pub const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const CHECK_TIMEOUT_SECS: u64 = 1;

pub use self::client::Message;
pub use self::local::start_local_tunnel_server;

pub use self::tls_client::new_tls_client;
pub use self::tls_remote::start_tls_remote_server;

pub async fn start_tunnel_client(
    url: &url::Url,
    cert_path: &std::path::Path,
    host: &str,
    idle_timeout_secs: usize,
    stream_window: u32,
    app_config: std::sync::Arc<crate::app_config::AppConfig>,
    max_age_secs: u64,
    concurrent: usize,
) -> anyhow::Result<()> {
    match url.scheme() {
        "tls" => {
            tunnel_client::start_tunnel_client_tls(
                url,
                cert_path,
                host,
                idle_timeout_secs,
                stream_window,
                app_config,
                max_age_secs,
                concurrent,
            )
            .await
        }
        "quic" => {
            start_tunnel_client_quic(url, cert_path, host, app_config, idle_timeout_secs, max_age_secs, concurrent).await
        }
        _ => Err(anyhow::anyhow!("unsupported scheme: {}", url.scheme())),
    }
}
