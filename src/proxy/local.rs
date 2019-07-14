use super::http::handle_http_connection;
use super::socks5::handle_socks5_connection;
use super::tls::handle_tls_connection;
use super::tls::valid_tls_version;
use crate::common::{peek_exact, tcp_split};
use crate::config::*;
use crate::mux::mux_relay_connection;
use crate::proxy::misc::*;

use std::net::SocketAddr;
use tokio::io::Error as TokioIOError;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

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
    pub target: Option<String>,
    pub default_port: u16,
}
impl LocalContext {
    pub fn is_transparent(&self) -> bool {
        //self.origin_dst != None
        get_config().lock().unwrap().local.transparent
    }
}

fn handle_local_connection(socket: TcpStream) -> impl Future<Item = (), Error = TokioIOError> {
    let origin_dst = get_origin_dst(&socket);
    let mut ctx: LocalContext = Default::default();
    ctx.origin_dst = origin_dst;
    // let mysocket = MyTcpStream::new(socket);
    // let (local_reader, local_writer) = mysocket.split();
    let (local_reader, local_writer) = tcp_split(socket);
    peek_exact(local_reader, [0u8; 3])
        .map_err(|(_r, e)| e)
        .and_then(move |(_reader, _data)| {
            match _data[0] {
                5 => {
                    //socks5
                    info!("Accept client as SOCKS5 proxy.");
                    tokio::spawn(handle_socks5_connection(ctx, _reader, local_writer));
                    return future::ok(());
                }
                4 => {
                    //socks4
                    error!("socks4 not supported!");
                    return future::err(other("unimplemented"));
                }
                _ => {
                    //info!("Not socks protocol:{}", _data[0]);
                }
            }
            if valid_tls_version(&_data[..]) {
                info!("Accept client as SNI proxy.");
                tokio::spawn(handle_tls_connection(ctx, _reader, local_writer));
                return future::ok(());
            }
            if let Ok(prefix_str) = std::str::from_utf8(&_data) {
                let prefix_str = prefix_str.to_uppercase();
                match prefix_str.as_str() {
                    "GET" | "PUT" | "POS" | "DEL" | "OPT" | "TRA" | "PAT" | "HEA" | "CON" => {
                        info!("Accept client as HTTP proxy with method:{}", prefix_str);
                        //http proxy
                        if prefix_str.as_str() == "CON" {
                            ctx.is_https = true;
                        }
                        tokio::spawn(handle_http_connection(ctx, _reader, local_writer));
                        return future::ok(());
                    }
                    _ => {
                        //nothing
                    }
                };
            }
            warn!(
                "unknnow tcp traffic to {:?} with first byte:{}",
                origin_dst, _data[0]
            );
            if let Some(dst) = origin_dst {
                let s = format!("{}:{}", dst.ip().to_string(), dst.port());
                //warn!("unknown tcp traffict to {}", s);
                let relay =
                    mux_relay_connection(_reader, local_writer, "tcp", s.as_str(), 30, None, true);
                tokio::spawn(relay);
                return future::ok(());
            } else {
                future::err(other("unsupported traffic"))
            }

            //future::err(other("unsupported traffic"))
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
