mod clean;
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

pub use clean::clean_rotate_logs;
#[cfg(unix)]
pub use daemon::daemonize;
#[cfg(windows)]
pub use daemon_windows::daemonize;
pub use error::make_io_error;
pub use io::fill_read_buf;
pub use metrics::format_metrics;
pub use metrics::MetricsRegistry;
pub use metrics::MetricsLogRecorder;
pub use net::get_original_dst;
pub use net::new_tcp_listener;
#[cfg(target_os = "linux")]
pub use net::new_udp_listener;
#[cfg(target_os = "linux")]
pub use udp::LinuxTproxyUdpSocket;

pub use tls::read_private_key;
pub use tls::read_tokio_tls_certs;
pub use udp::{UdpClientStream, UdpServerStream};

#[allow(dead_code)]
pub const MAXIMUM_UDP_PAYLOAD_SIZE: usize = 65536;
