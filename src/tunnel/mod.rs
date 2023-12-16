mod client;
mod http_local;
mod local;

mod socks5_local;
mod stream;
mod tls_client;
mod tls_local;
mod tls_remote;

mod transparent;

#[cfg(all(feature = "quinn", not(feature = "s2n_quic")))]
mod quinn_quic_client;
#[cfg(all(feature = "quinn", not(feature = "s2n_quic")))]
pub use self::quinn_quic_client::new_quic_client;

#[cfg(all(feature = "quinn", not(feature = "s2n_quic")))]
mod quinn_quic_remote;
#[cfg(all(feature = "quinn", not(feature = "s2n_quic")))]
pub use self::quinn_quic_remote::start_quic_remote_server;

#[cfg(feature = "s2n_quic")]
mod s2n_quic_client;
#[cfg(feature = "s2n_quic")]
pub use self::s2n_quic_client::new_quic_client;

#[cfg(feature = "s2n_quic")]
mod s2n_quic_remote;
#[cfg(feature = "s2n_quic")]
pub use self::s2n_quic_remote::start_quic_remote_server;

// pub const DEFAULT_TLS_HOST: &str = "google.com";
pub const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const CHECK_TIMEOUT_SECS: u64 = 5;

pub use self::client::Message;
pub use self::local::start_local_tunnel_server;

pub use self::tls_client::new_tls_client;
pub use self::tls_remote::start_tls_remote_server;
