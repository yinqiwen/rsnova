use nix::sys::socket::{getsockopt, sockopt, InetAddr};
use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;
use tokio::net::TcpStream;

#[cfg(not(any(target_os = "android", target_os = "linux")))]
pub fn get_origin_dst(socket: &TcpStream) -> Option<SocketAddr> {
    None
}

#[cfg(any(target_os = "android", target_os = "linux"))]
pub fn get_origin_dst(socket: &TcpStream) -> Option<SocketAddr> {
    let fd = socket.as_raw_fd();
    let opt = sockopt::OriginalDst {};
    match getsockopt(fd, opt) {
        Ok(addr) => Some(InetAddr::V4(addr).to_std()),
        Err(_) => None,
    }
}

// #[cfg(not(target_os = "linux"))]
// pub fn get_origin_dst() {}
