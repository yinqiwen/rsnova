use super::relay::relay_connection;
use crate::utils::make_error;

use crate::config::TunnelConfig;
use std::error::Error;
use std::net::{Ipv4Addr, Ipv6Addr};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

mod v5 {
    pub const VERSION: u8 = 5;

    pub const METH_NO_AUTH: u8 = 0;
    pub const METH_GSSAPI: u8 = 1;
    pub const METH_USER_PASS: u8 = 2;

    pub const CMD_CONNECT: u8 = 1;
    pub const CMD_BIND: u8 = 2;
    pub const CMD_UDP_ASSOCIATE: u8 = 3;

    pub const ATYP_IPV4: u8 = 1;
    pub const ATYP_IPV6: u8 = 4;
    pub const ATYP_DOMAIN: u8 = 3;

    pub const SOCKS_RESP_SUUCESS: u8 = 0;
}

// Extracts the name and port from addr_buf and returns them, converting
// the name to the form that the trust-dns client can use. If the original
// name can be parsed as an IP address, makes a SocketAddr from that
// address and the port and returns it; we skip DNS resolution in that
// case.
fn name_port(addr_buf: &[u8]) -> Option<String> {
    // The last two bytes of the buffer are the port, and the other parts of it
    // are the hostname.
    let hostname = &addr_buf[..addr_buf.len() - 2];
    let hostname = match std::str::from_utf8(hostname) {
        Ok(s) => s,
        Err(_e) => {
            return None;
        }
    };
    let pos = addr_buf.len() - 2;
    let port = ((addr_buf[pos] as u16) << 8) | (addr_buf[pos + 1] as u16);
    Some(format!("{}:{}", hostname, port))
}

pub async fn handle_socks5(
    tunnel_id: u32,
    mut inbound: TcpStream,
    cfg: &TunnelConfig,
) -> Result<(), Box<dyn Error>> {
    //let mut peek_buf = Vec::new();
    let mut num_methods_buf = [0u8; 2];
    inbound.read_exact(&mut num_methods_buf).await?;
    let mut vdata = vec![0; num_methods_buf[1] as usize];
    inbound.read_exact(&mut vdata).await?;
    if !vdata.contains(&v5::METH_NO_AUTH) {
        return Err(make_error("no supported method given"));
    }
    inbound.write_all(&[v5::VERSION, v5::METH_NO_AUTH]).await?;
    let mut head = [0u8; 4];
    inbound.read_exact(&mut head).await?;
    if head[0] != v5::VERSION {
        return Err(make_error("didn't confirm with v5 version"));
    }
    if head[1] != v5::CMD_CONNECT {
        return Err(make_error("unsupported command"));
    }
    let target_addr = match head[3] {
        v5::ATYP_IPV4 => {
            let mut addr_buf = [0u8; 6];
            inbound.read_exact(&mut addr_buf).await?;
            let addr = Ipv4Addr::new(addr_buf[0], addr_buf[1], addr_buf[2], addr_buf[3]);
            let port = ((addr_buf[4] as u16) << 8) | (addr_buf[5] as u16);
            format!("{}:{}", addr.to_string(), port)
        }
        v5::ATYP_IPV6 => {
            let mut addr_buf = [0u8; 18];
            inbound.read_exact(&mut addr_buf).await?;
            let a = ((addr_buf[0] as u16) << 8) | (addr_buf[1] as u16);
            let b = ((addr_buf[2] as u16) << 8) | (addr_buf[3] as u16);
            let c = ((addr_buf[4] as u16) << 8) | (addr_buf[5] as u16);
            let d = ((addr_buf[6] as u16) << 8) | (addr_buf[7] as u16);
            let e = ((addr_buf[8] as u16) << 8) | (addr_buf[9] as u16);
            let f = ((addr_buf[10] as u16) << 8) | (addr_buf[11] as u16);
            let g = ((addr_buf[12] as u16) << 8) | (addr_buf[13] as u16);
            let h = ((addr_buf[14] as u16) << 8) | (addr_buf[15] as u16);
            let addr = Ipv6Addr::new(a, b, c, d, e, f, g, h);
            let port = ((addr_buf[16] as u16) << 8) | (addr_buf[17] as u16);
            format!("{}:{}", addr.to_string(), port)
        }
        v5::ATYP_DOMAIN => {
            //
            let mut len_buf = [0u8; 1];
            inbound.read_exact(&mut len_buf).await?;
            let mut addr_buf = vec![0u8; len_buf[0] as usize + 2];
            inbound.read_exact(&mut addr_buf).await?;
            match name_port(&addr_buf) {
                Some(addr) => addr,
                None => {
                    return Err(make_error("can not get addr with domian"));
                }
            }
        }
        n => {
            let msg = format!("unknown ATYP received: {}", n);
            return Err(make_error(msg.as_str()));
        }
    };
    let mut resp = [0u8; 10];
    // VER - protocol version
    resp[0] = 5;
    // REP - "reply field" -- what happened with the actual connect.
    //
    // In theory this should reply back with a bunch more kinds of
    // errors if possible, but for now we just recognize a few concrete
    // errors.
    resp[1] = v5::SOCKS_RESP_SUUCESS;

    // RSV - reserved
    resp[2] = 0;
    resp[3] = 1; // socksAtypeV4         = 0x01
    inbound.write_all(&resp).await?;

    info!(
        "[{}]Handle SOCKS5 proxy to {} with local:{} remote:{}",
        tunnel_id,
        target_addr,
        inbound.local_addr().unwrap(),
        inbound.peer_addr().unwrap()
    );
    relay_connection(tunnel_id, inbound, cfg, target_addr, Vec::new()).await?;
    Ok(())
}
