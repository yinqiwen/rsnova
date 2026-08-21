use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;

/// Bounded exponential delay for retrying transient listener accept errors.
pub struct AcceptBackoff {
    current: Duration,
}

impl Default for AcceptBackoff {
    fn default() -> Self {
        Self {
            current: Duration::from_millis(10),
        }
    }
}

impl AcceptBackoff {
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current * 2).min(Duration::from_secs(1));
        delay
    }

    pub fn reset(&mut self) {
        self.current = Duration::from_millis(10);
    }
}

/// Idle time before the first TCP keepalive probe on mux TLS sockets.
pub const TCP_KEEPALIVE_IDLE: Duration = Duration::from_secs(30);

/// Tune a mux TLS socket for tunnel traffic.
///
/// * `TCP_NODELAY`: this mux carries many small control frames (PING/PONG,
///   WINDOW_UPDATE, SOCKS5 handshakes) interleaved with bulk DATA. Nagle
///   would hold small frames up to 40-200ms on lossy paths.
/// * TCP keepalive (30s idle / 10s interval / 3 probes ≈ 60s detection):
///   reclaims half-open links where the peer vanished without FIN.
///
/// Failure to set keepalive is logged but not fatal.
pub fn set_tcp_keepalive(stream: &TcpStream) {
    if let Err(e) = stream.set_nodelay(true) {
        tracing::warn!("set_nodelay failed: {}", e);
    }
    let sock_ref = socket2::SockRef::from(stream);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE_IDLE)
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    if let Err(e) = sock_ref.set_tcp_keepalive(&keepalive) {
        tracing::warn!("set_tcp_keepalive failed: {}", e);
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn accept_backoff_is_bounded_and_resets_after_success() {
        let mut backoff = AcceptBackoff::default();
        assert_eq!(backoff.next_delay(), Duration::from_millis(10));
        assert_eq!(backoff.next_delay(), Duration::from_millis(20));
        for _ in 0..16 {
            let _ = backoff.next_delay();
        }
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(10));
    }

    #[tokio::test]
    async fn set_tcp_keepalive_enables_so_keepalive_with_30s_idle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = tokio::net::TcpStream::connect(addr);
        let accept = listener.accept();
        let (client, accepted) = tokio::join!(connect, accept);
        let client = client.unwrap();
        let (server, _) = accepted.unwrap();

        set_tcp_keepalive(&client);
        set_tcp_keepalive(&server);

        assert!(
            socket2::SockRef::from(&client).keepalive().unwrap(),
            "client SO_KEEPALIVE should be on"
        );
        assert!(
            socket2::SockRef::from(&server).keepalive().unwrap(),
            "server SO_KEEPALIVE should be on"
        );

        assert_eq!(keepalive_idle_secs(&client).unwrap(), 30);
        assert_eq!(keepalive_idle_secs(&server).unwrap(), 30);
    }

    fn keepalive_idle_secs(stream: &tokio::net::TcpStream) -> std::io::Result<u32> {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = stream.as_raw_fd();
            let mut idle: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            let opt = {
                #[cfg(target_os = "linux")]
                {
                    libc::TCP_KEEPIDLE
                }
                #[cfg(not(target_os = "linux"))]
                {
                    libc::TCP_KEEPALIVE
                }
            };
            let ret = unsafe {
                libc::getsockopt(
                    fd,
                    libc::IPPROTO_TCP,
                    opt,
                    &mut idle as *mut _ as *mut libc::c_void,
                    &mut len,
                )
            };
            if ret == 0 {
                Ok(idle as u32)
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
        #[cfg(not(unix))]
        {
            let _ = stream;
            Ok(30)
        }
    }
}
