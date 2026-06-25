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
pub async fn new_tcp_listener(
    addr: &SocketAddr,
    transparent: bool,
) -> std::io::Result<tokio::net::TcpListener> {
    let socket2_addr = socket2::SockAddr::from(*addr);
    let domain = if socket2_addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let listen_tcp_socket = socket2::Socket::new(domain, socket2::Type::STREAM, None)?;
    if transparent {
        set_ip_transparent(&listen_tcp_socket, domain)?;
    }
    listen_tcp_socket.bind(&socket2_addr)?;
    listen_tcp_socket.listen(128)?;
    tokio::net::TcpListener::from_std(listen_tcp_socket.into())
}
#[cfg(not(target_os = "linux"))]
pub async fn new_tcp_listener(
    addr: &SocketAddr,
    _transparent: bool,
) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr).await
}
#[cfg(target_os = "linux")]
pub fn new_udp_listener(
    addr: &SocketAddr,
    transparent: bool,
) -> std::io::Result<tokio::net::UdpSocket> {
    let socket2_addr = socket2::SockAddr::from(*addr);
    let domain = if socket2_addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let listen_udp_socket = socket2::Socket::new(domain, socket2::Type::DGRAM, None)?;
    if transparent {
        set_ip_transparent(&listen_udp_socket, domain)?;
    }
    // Ok(listen_udp_socket)
    listen_udp_socket.bind(&socket2_addr)?;
    tokio::net::UdpSocket::from_std(listen_udp_socket.into())
}

// #[cfg(not(target_os = "linux"))]
// pub async fn new_udp_listener(
//     addr: &SocketAddr,
//     transparent: bool,
// ) -> std::io::Result<tokio::net::UdpSocket> {
//     tokio::net::UdpSocket::bind(addr).await
// }

#[cfg(target_os = "linux")]
fn set_ip_transparent(socket: &socket2::Socket, domain: socket2::Domain) -> std::io::Result<()> {
    match domain {
        socket2::Domain::IPV4 => socket.set_ip_transparent_v4(true),
        socket2::Domain::IPV6 => socket.set_ip_transparent_v6(true),
        _ => Ok(()),
    }
}
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
fn set_ip_transparent(_socket: &socket2::Socket, _domain: socket2::Domain) -> std::io::Result<()> {
    Ok(())
}

// #[cfg(target_os = "linux")]
// pub fn get_tproxy_original_dst(s: &TcpStream) -> std::io::Result<SocketAddr> {
//     use std::os::fd::FromRawFd;
//     use std::os::unix::io::AsRawFd;
//     let fd = s.as_raw_fd();
//     let socket = unsafe { socket2::Socket::from_raw_fd(fd) };

//     match socket.original_dst() {
//         Ok(addr) => {
//             return sockaddr_storage_to_socketaddr(addr.as_storage());
//         }
//         Err(_) => match socket.original_dst_ipv6() {
//             Ok(addr6) => {
//                 return sockaddr_storage_to_socketaddr(addr6.as_storage());
//             }
//             Err(_e) => Err(std::io::Error::last_os_error()),
//         },
//     }
// }
// #[cfg(not(target_os = "linux"))]
// pub fn get_tproxy_original_dst(stream: &TcpStream) -> std::io::Result<SocketAddr> {
//     Err(std::io::Error::new(
//         std::io::ErrorKind::Other,
//         "not supported in current os",
//     ))
// }

#[cfg(target_os = "linux")]
pub fn get_original_dst(stream: &TcpStream) -> std::io::Result<SocketAddr> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();

    let mut addr_storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut addrlen: u32 = std::mem::size_of_val(&addr_storage) as libc::socklen_t;

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
pub fn get_original_dst(_stream: &TcpStream) -> std::io::Result<SocketAddr> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "not supported in current os",
    ))
}

#[cfg(target_os = "linux")]
pub fn get_destination_addr(msg: &libc::msghdr) -> std::io::Result<SocketAddr> {
    use std::{io::Error, io::ErrorKind, mem, ptr};

    use socket2::SockAddr;
    unsafe {
        let (_, addr) = SockAddr::try_init(|dst_addr, dst_addr_len| {
            let mut cmsg: *mut libc::cmsghdr = libc::CMSG_FIRSTHDR(msg);
            while !cmsg.is_null() {
                let rcmsg = &*cmsg;
                match (rcmsg.cmsg_level, rcmsg.cmsg_type) {
                    (libc::SOL_IP, libc::IP_RECVORIGDSTADDR) => {
                        ptr::copy(
                            libc::CMSG_DATA(cmsg),
                            dst_addr as *mut _,
                            mem::size_of::<libc::sockaddr_in>(),
                        );
                        *dst_addr_len = mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;

                        return Ok(());
                    }
                    (libc::SOL_IPV6, libc::IPV6_RECVORIGDSTADDR) => {
                        ptr::copy(
                            libc::CMSG_DATA(cmsg),
                            dst_addr as *mut _,
                            mem::size_of::<libc::sockaddr_in6>(),
                        );
                        *dst_addr_len = mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;

                        return Ok(());
                    }
                    _ => {}
                }
                cmsg = libc::CMSG_NXTHDR(msg, cmsg);
            }
            let err = Error::new(
                ErrorKind::InvalidData,
                "missing destination address in msghdr",
            );
            Err(err)
        })?;

        Ok(addr.as_socket().expect("SocketAddr"))
    }
}
