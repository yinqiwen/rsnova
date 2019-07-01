use std::io as std_io;
use std::io::ErrorKind;
use std::net::AddrParseError;
use std::net::SocketAddr;
use std::net::UdpSocket;
use tokio::net::TcpStream;
use url::Url;

use futures::future::Either;
use futures::Future;
use std::net::ToSocketAddrs;

use super::{is_ok_response, read_until_separator};
use tokio_io::io::write_all;

pub fn get_hostport_from_url(url: &Url) -> Option<String> {
    let mut hostport = String::new();
    if let Some(host) = url.host_str() {
        hostport.push_str(host);
    } else {
        // Destructure failed. Change to the failure case.
        error!("no host found in url:{}", url);
        return None;
    };
    hostport.push_str(":");
    if let Some(port) = url.port() {
        hostport.push_str(port.to_string().as_str());
    } else {
        match url.scheme() {
            "http" => hostport.push_str("80"),
            "https" => hostport.push_str("443"),
            "tcp" => hostport.push_str("48100"),
            "tls" => hostport.push_str("443"),
            _ => {
                hostport.push_str("48100");
                warn!("use default '48100' port for scheme:{}", url.scheme());
            }
        }
    };
    Some(hostport)
}

pub fn get_listen_addr(addr: &str) -> Result<SocketAddr, AddrParseError> {
    let mut laddr = addr.to_owned();
    if addr.chars().nth(0).unwrap() == ':' {
        laddr = "0.0.0.0".to_owned();
        laddr.push_str(addr);
    }
    laddr.parse()
}

fn udp_port_is_available(port: u16) -> bool {
    UdpSocket::bind(("0.0.0.0", port)).is_ok()
}

pub fn get_available_udp_port() -> u16 {
    match (1025..65535).find(|port| udp_port_is_available(*port)) {
        Some(n) => n,
        None => 0,
    }
}

pub fn http_proxy_connect(
    proxy: &str,
    remote: &str,
) -> impl Future<Item = TcpStream, Error = std_io::Error> {
    let connect_str = format!(
        "CONNECT {} HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\nProxy-Connection: keep-alive\r\n\r\n",
        remote, remote
    );
    let connect = connect_str.into_bytes();
    let raddr: Vec<SocketAddr> = match proxy.to_socket_addrs() {
        Ok(m) => m.collect(),
        Err(err) => {
            error!(
                "Failed to parse addr with error:{} from connect request:{}",
                err, proxy
            );
            return Either::A(futures::future::err(std_io::Error::from(
                ErrorKind::ConnectionAborted,
            )));
        }
    };
    Either::B(TcpStream::connect(&raddr[0]).and_then(|sock| {
        write_all(sock, connect)
            .and_then(|(c, _)| read_until_separator(c, "\r\n\r\n"))
            .and_then(|(c, b0, _)| {
                if !is_ok_response(&b0[..]) {
                    Err(std_io::Error::from(ErrorKind::PermissionDenied))
                } else {
                    Ok(c)
                }
            })
    }))
}
