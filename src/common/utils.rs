use bytes::BytesMut;
use std::net::AddrParseError;
use std::net::SocketAddr;
use std::net::UdpSocket;
use url::Url;

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
    return Some(hostport);
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
    match UdpSocket::bind(("0.0.0.0", port)) {
        Ok(_) => true,
        Err(_) => false,
    }
}

pub fn get_available_udp_port() -> u16 {
    match (1025..65535).find(|port| udp_port_is_available(*port)) {
        Some(n) => n,
        None => 0,
    }
}

pub fn find_str_in_bytes(buf: &BytesMut, s: &str) -> Option<usize> {
    let mut i = 0;
    let b = buf.as_ref();
    while i < buf.len() {
        let c = b[i] as char;
        if s.find(c) != None {
            if i + s.len() < buf.len() {
                if let Ok(ss) = std::str::from_utf8(&b[i..(i + s.len())]) {
                    if ss == s {
                        return Some(i);
                    }
                }
            } else {
                return None;
            }
            i = i + 1;
        } else {
            i = i + s.len();
        }
    }
    None
}
