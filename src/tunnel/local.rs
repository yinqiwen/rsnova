//use super::tls::handle_tls;
use super::socks5::handle_socks5;
use super::tls::handle_tls;
use super::tls::valid_tls_version;

use futures::FutureExt;
use std::error::Error;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

use std::sync::atomic::{AtomicU32, Ordering};

use crate::config::TunnelConfig;

pub fn make_error(desc: &str) -> Box<dyn Error> {
    Box::new(std::io::Error::new(std::io::ErrorKind::Other, desc))
}

async fn handle_inbound(tunnel_id: u32, mut inbound: TcpStream) -> Result<(), Box<dyn Error>> {
    let mut peek_buf = [0u8; 3];
    inbound.peek(&mut peek_buf).await?;
    match peek_buf[0] {
        5 => {
            //socks5
            info!("[{}]Accept client as SOCKS5 proxy.", tunnel_id);
            handle_socks5(tunnel_id, inbound).await?;
            return Ok(());
        }
        4 => {
            //socks4
            error!("socks4 not supported!");
            return Err(make_error("socks4 unimplemented"));
        }
        _ => {
            //info!("Not socks protocol:{}", _data[0]);
        }
    }
    if valid_tls_version(&peek_buf[..]) {
        info!("[{}]Accept client as SNI proxy.", tunnel_id);
        handle_tls(tunnel_id, inbound).await?;
        return Ok(());
    }
    if let Ok(prefix_str) = std::str::from_utf8(&peek_buf) {
        let prefix_str = prefix_str.to_uppercase();
        match prefix_str.as_str() {
            "GET" | "PUT" | "POS" | "DEL" | "OPT" | "TRA" | "PAT" | "HEA" | "CON" => {
                info!("Accept client as HTTP proxy with method:{}", prefix_str);
                //http proxy
                if prefix_str.as_str() == "CON" {
                    //ctx.is_https = true;
                }
            }
            _ => {
                //nothing
            }
        };
    }

    Ok(())
}

pub async fn start_local_server(cfg: TunnelConfig) -> Result<(), Box<dyn Error>> {
    let mut listener = TcpListener::bind(cfg.listen.as_str()).await?;
    let tunnel_id_seed = AtomicU32::new(0);
    while let Ok((inbound, _)) = listener.accept().await {
        let tunnel_id = tunnel_id_seed.fetch_add(1, Ordering::SeqCst);
        let handle = handle_inbound(tunnel_id, inbound).map(move |r| {
            if let Err(e) = r {
                error!("[{}]Failed to handle; error={}", tunnel_id, e);
            }
        });
        tokio::spawn(handle);
    }
    Ok(())
}
