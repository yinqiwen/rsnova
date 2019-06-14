use crate::common::future::FourEither;
use crate::common::io::HostPort;
use crate::proxy::local::*;

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use std::io::ErrorKind;
use tokio::codec::{Decoder, Encoder};
use tokio::net::TcpStream;
use tokio::prelude::*;
use tokio_io::io::{read_exact, write_all, Window};

use tokio_io::{AsyncRead, AsyncWrite};

use futures::future::Either;

use url::{Host, HostAndPort, Url};

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

    pub const SOCKS_RESP_SUUCESS:u8 = 0;
}

fn other(desc: &str) -> std::io::Error {
    std::io::Error::new(ErrorKind::Other, desc)
}


// Extracts the name and port from addr_buf and returns them, converting
// the name to the form that the trust-dns client can use. If the original
// name can be parsed as an IP address, makes a SocketAddr from that
// address and the port and returns it; we skip DNS resolution in that
// case.
fn name_port(addr_buf: &[u8]) -> std::io::Result<HostPort> {
    // The last two bytes of the buffer are the port, and the other parts of it
    // are the hostname.
    let hostname = &addr_buf[..addr_buf.len() - 2];
    let hostname = try!(std::str::from_utf8(hostname).map_err(|_e| {
        other("hostname buffer provided was not valid utf-8")
    }));
    let pos = addr_buf.len() - 2;
    let port = ((addr_buf[pos] as u16) << 8) | (addr_buf[pos + 1] as u16);
    Ok(HostPort::DomainPort(String::from(hostname), port))
}

pub fn handle_socks5_connection<R, W>(mut ctx: LocalContext, reader: R, writer: W)
where
    R: AsyncRead,
    W: AsyncWrite{
    let num_methods = read_exact(conn, [0u8]);
    let authenticated = num_methods
        .and_then(|(conn, buf)| read_exact(conn, vec![0u8; buf[0] as usize]))
        .and_then(|(conn, buf)| {
            if buf.contains(&v5::METH_NO_AUTH) {
                Ok(conn)
            } else {
                Err(other("no supported method given"))
            }
        });
    // After we've concluded that one of the client's supported methods is
    // `METH_NO_AUTH`, we "ack" this to the client by sending back that
    // information. Here we make use of the `write_all` combinator which
    // works very similarly to the `read_exact` combinator.
    let part1 = authenticated.and_then(|conn| write_all(conn, [v5::VERSION, v5::METH_NO_AUTH]));

    // Next up, we get a selected protocol version back from the client, as
    // well as a command indicating what they'd like to do. We just verify
    // that the version is still v5, and then we only implement the
    // "connect" command so we ensure the proxy sends that.
    //
    // As above, we're using `and_then` not only for chaining "blocking
    // computations", but also to perform fallible computations.
    let ack = part1.and_then(|(conn, _)| {
        read_exact(conn, [0u8]).and_then(|(conn, buf)| {
            if buf[0] == v5::VERSION {
                Ok(conn)
            } else {
                Err(other("didn't confirm with v5 version"))
            }
        })
    });
    let command = ack.and_then(|conn| {
        read_exact(conn, [0u8]).and_then(|(conn, buf)| {
            if buf[0] == v5::CMD_CONNECT {
                Ok(conn)
            } else {
                Err(other("unsupported command"))
            }
        })
    });
    let resv = command.and_then(|c| read_exact(c, [0u8]).map(|c| c.0));
    let atyp = resv.and_then(|c| read_exact(c, [0u8]));
    let read_addr = atyp.and_then(move |(c, buf)| {
        match buf[0] {
            // For IPv4 addresses, we read the 4 bytes for the address as
            // well as 2 bytes for the port.
            v5::ATYP_IPV4 => {
                //
                FourEither::A(read_exact(c, [0u8; 6]).and_then(|(c, buf)| {
                    let addr = Ipv4Addr::new(buf[0], buf[1], buf[2], buf[3]);
                    let port = ((buf[4] as u16) << 8) | (buf[5] as u16);
                    let addr = SocketAddrV4::new(addr, port);
                    Ok((c, HostPort::IPPort(SocketAddr::V4(addr))))
                }))
            }

            v5::ATYP_IPV6 => {
                //
                FourEither::B(read_exact(c, [0u8; 18]).and_then(|(conn, buf)| {
                    let a = ((buf[0] as u16) << 8) | (buf[1] as u16);
                    let b = ((buf[2] as u16) << 8) | (buf[3] as u16);
                    let c = ((buf[4] as u16) << 8) | (buf[5] as u16);
                    let d = ((buf[6] as u16) << 8) | (buf[7] as u16);
                    let e = ((buf[8] as u16) << 8) | (buf[9] as u16);
                    let f = ((buf[10] as u16) << 8) | (buf[11] as u16);
                    let g = ((buf[12] as u16) << 8) | (buf[13] as u16);
                    let h = ((buf[14] as u16) << 8) | (buf[15] as u16);
                    let addr = Ipv6Addr::new(a, b, c, d, e, f, g, h);
                    let port = ((buf[16] as u16) << 8) | (buf[17] as u16);
                    let addr = SocketAddrV6::new(addr, port, 0, 0);
                    Ok((conn, HostPort::IPPort(SocketAddr::V6(addr))))
                }))
            }
            v5::ATYP_DOMAIN => {
                //
                FourEither::C(
                    read_exact(c, [0u8]).and_then(|(conn, buf)| {
                        read_exact(conn, vec![0u8; buf[0] as usize + 2])
                    }).and_then(move |(conn, buf)| {
                        match name_port(&buf) {
                            Ok(addr) =>  Ok((conn, addr)),
                            Err(e) => Err(e),
                        }
                    })
                )
            }
            n => {
                let msg = format!("unknown ATYP received: {}", n);
                FourEither::D(future::err(other(&msg)))
            }
        }
    });

    let grant = read_addr.and_then(|(c, addr)|{
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
            // BND.ADDR/BND.PORT is always theIPv4 address/port "0.0.0.0:0".
            write_all(c, resp).map(|c|{
                (c.0, addr)
            })
    });

    
}
