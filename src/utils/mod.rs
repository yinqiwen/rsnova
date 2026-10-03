// These re-exports serve the binary target (src/main.rs declares `mod utils`
// and consumes them via `crate::utils::*`). The library facade (`src/lib.rs`)
// only exposes `mux`, so under the lib target these imports appear unused.
// Suppress the noise rather than weakening the module's public surface.
#![allow(unused_imports, dead_code)]

#[cfg(unix)]
mod daemon;
#[cfg(windows)]
mod daemon_windows;
mod error;
mod io;
mod metrics;
mod net;
mod tls;
mod udp;

#[cfg(unix)]
pub use daemon::daemonize;
#[cfg(windows)]
pub use daemon_windows::daemonize;
pub use error::make_io_error;
pub use io::fill_read_buf;
pub use metrics::MetricsLogRecorder;
pub use metrics::MetricsRegistry;
pub use metrics::format_metrics;
pub use net::AcceptBackoff;
pub use net::get_original_dst;
pub use net::new_tcp_listener;
#[cfg(target_os = "linux")]
pub use net::new_udp_listener;
pub use net::set_tcp_keepalive;
#[cfg(target_os = "linux")]
pub use udp::LinuxTproxyUdpSocket;

pub use tls::read_private_key;
pub use tls::read_tokio_tls_certs;
pub use udp::{UdpClientStream, UdpServerStream};

#[allow(dead_code)]
pub const MAXIMUM_UDP_PAYLOAD_SIZE: usize = 65536;
