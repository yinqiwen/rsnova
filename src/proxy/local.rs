use crate::common::io::*;
use crate::mux::channel::*;
use crate::mux::mux::*;
use crate::proxy::http::*;
use crate::proxy::misc::*;

use bytes::BytesMut;

use std::net::SocketAddr;
use url::{Host, HostAndPort, Url};

use tokio::io::Error as TokioIOError;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio_io::io::ReadHalf;
use tokio_io::io::WriteHalf;
use tokio_io::AsyncRead;

use tokio_io::io::{read_exact, read_until};

use std::io::ErrorKind;
use tokio::prelude::*;

fn other(desc: &str) -> TokioIOError {
    TokioIOError::new(ErrorKind::Other, desc)
}

#[derive(Debug, Default)]
pub struct LocalContext {
    pub origin_dst: Option<SocketAddr>,
    pub is_https: bool,
    pub is_socks: bool,
    pub target: Option<HostAndPort>,
    pub default_port: u16,
}
impl LocalContext {
    fn is_transparent(&self) -> bool {
        self.origin_dst != None
    }
}

fn handle_local_connection(socket: TcpStream) -> impl Future<Item = (), Error = TokioIOError> {
    let origin_dst = get_origin_dst(&socket);
    let mut ctx: LocalContext = Default::default();
    ctx.origin_dst = origin_dst;
    let (local_reader, local_writer) = socket.split();
    peek_exact(local_reader, [0u8; 3]).and_then(move |(_reader, _data)| {
        if ctx.origin_dst == None {
            match _data[0] {
                5 => {
                    //socks5
                }
                4 => {
                    //socks4
                    error!("socks4 not supported!");
                    return future::err(other("unimplemented"));
                }
                _ => {
                    //continue
                }
            }
        }
        if let Ok(prefix_str) = std::str::from_utf8(&_data) {
            let prefix_str = prefix_str.to_uppercase();
            match prefix_str.as_str() {
                "GET" | "PUT" | "POS" | "DEL" | "OPT" | "TRA" | "PAT" | "HEA" | "CON" => {
                    //http proxy
                    if prefix_str.as_str() == "CON" {
                        ctx.is_https = true;
                    }
                    handle_http_connection(ctx, _reader, local_writer);
                    //tokio::spawn(task);
                }
                _ => {
                    //try tls proxy
                }
            };
        }
        future::ok(())
    })
}

pub fn start_local_server(addr: &str) {
    let mut laddr = addr.to_owned();
    if addr.chars().nth(0).unwrap() == ':' {
        laddr = "0.0.0.0".to_owned();
        laddr.push_str(addr);
    }
    info!("Local server listen on {}", laddr);
    let net_addr = laddr.parse().unwrap();
    let listener = TcpListener::bind(&net_addr).expect("unable to bind TCP listener");

    // Pull out a stream of sockets for incoming connections
    let server = listener
        .incoming()
        .map_err(|e| error!("accept failed = {:?}", e))
        .for_each(|sock| {
            let handle_conn = handle_local_connection(sock);
            tokio::spawn(handle_conn.map_err(|e| {
                error!("Failed to handle local conn with error:{}", e);
            }))
        });

    // Start the Tokio runtime
    tokio::spawn(server);
}
