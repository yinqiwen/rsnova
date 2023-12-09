mod client;
mod http_local;
mod local;
mod quic_remote;
mod socks5_local;
mod stream;
mod tls_local;
mod tls_remote;

// pub const DEFAULT_TLS_HOST: &str = "google.com";
pub const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];
pub const DefaultTimeoutSecs: u64 = 30;

pub use self::client::Message;
pub use self::client::MuxClient;
pub use self::client::QuicInnerConnection;
pub use self::client::TlsInnerConnection;
pub use self::local::start_local_tunnel_server;
pub use self::quic_remote::start_quic_remote_server;
pub use self::tls_remote::start_tls_remote_server;
