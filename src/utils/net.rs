use std::net::SocketAddr;
use tokio::net::TcpStream;

#[cfg(target_os = "linux")]
fn sockaddr_storage_to_socketaddr(
    addr_storage: libc::sockaddr_storage,
) -> std::io::Result<SocketAddr> {
    match addr_storage.ss_family as libc::c_int {
        libc::AF_INET => {
            let addr_in: &libc::sockaddr_in = unsafe { std::mem::transmute(&addr_storage) };
            let ip = std::net::Ipv4Addr::from(addr_in.sin_addr.s_addr.to_be());
            let port = u16::from_be(addr_in.sin_port);
            Ok(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                ip, port,
            )))
        }
        libc::AF_INET6 => {
            let addr_in6: &libc::sockaddr_in6 = unsafe { std::mem::transmute(&addr_storage) };
            let ip = std::net::Ipv6Addr::from(addr_in6.sin6_addr.s6_addr);
            let port = u16::from_be(addr_in6.sin6_port);
            Ok(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                ip,
                port,
                addr_in6.sin6_flowinfo,
                addr_in6.sin6_scope_id,
            )))
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid address family",
        )),
    }
}

#[cfg(target_os = "linux")]
pub fn get_original_dst(stream: &TcpStream) -> std::io::Result<SocketAddr> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();

    let mut addr_storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut addrlen = std::mem::size_of_val(&addr_storage) as libc::socklen_t;

    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IP,
            libc::SO_ORIGINAL_DST,
            &mut addr_storage as *mut _ as *mut libc::c_void,
            &mut addrlen,
        )
    };

    if ret == 0 {
        sockaddr_storage_to_socketaddr(addr_storage)
    } else {
        let ret = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_IPV6,
                libc::IP6T_SO_ORIGINAL_DST,
                &mut addr_storage as *mut _ as *mut libc::c_void,
                &mut addrlen,
            )
        };
        if ret == 0 {
            sockaddr_storage_to_socketaddr(addr_storage)
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn get_original_dst(stream: &TcpStream) -> std::io::Result<SocketAddr> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "not supported in current os",
    ))
}
