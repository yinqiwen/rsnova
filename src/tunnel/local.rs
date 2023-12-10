use std::net::SocketAddr;

use crate::tunnel::http_local::{handle_http, handle_https};
use crate::tunnel::socks5_local::handle_socks5;
use crate::tunnel::tls_local::{handle_tls, valid_tls_version};
use crate::tunnel::Message;
use anyhow::{anyhow, Result};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

async fn handle_local_tunnel(
    inbound: TcpStream,
    tunnel_id: u32,
    sender: mpsc::UnboundedSender<Message>,
) -> Result<()> {
    //stream.peek(buf)
    let mut peek_buf = [0u8; 3];
    inbound.peek(&mut peek_buf).await?;
    match peek_buf[0] {
        5 => {
            //socks5
            tracing::info!("[{}]Accept client as SOCKS5 proxy.", tunnel_id);
            handle_socks5(tunnel_id, inbound, sender).await?;
            return Ok(());
        }
        4 => {
            //socks4
            tracing::error!("socks4 not supported!");
            return Err(anyhow!("socks4 unimplemented"));
        }
        _ => {
            //info!("Not socks protocol:{}", _data[0]);
        }
    }
    if valid_tls_version(&peek_buf[..]) {
        tracing::info!("[{}]Accept client as TLS proxy.", tunnel_id);
        handle_tls(tunnel_id, inbound, sender).await?;
        return Ok(());
    }
    if let Ok(prefix_str) = std::str::from_utf8(&peek_buf) {
        let prefix_str = prefix_str.to_uppercase();
        match prefix_str.as_str() {
            "GET" | "PUT" | "POS" | "DEL" | "OPT" | "TRA" | "PAT" | "HEA" | "CON" | "UPG" => {
                tracing::info!(
                    "[{}]Accept client as HTTP proxy with method:{}",
                    tunnel_id,
                    prefix_str
                );
                //http proxy
                if prefix_str.as_str() == "CON" {
                    handle_https(tunnel_id, inbound, sender).await?;
                } else {
                    handle_http(tunnel_id, inbound, sender).await?;
                }
                return Ok(());
            }
            _ => {
                //nothing
            }
        };
    }
    Ok(())
}

pub async fn start_local_tunnel_server(
    addr: &SocketAddr,
    sender: mpsc::UnboundedSender<Message>,
) -> Result<(), std::io::Error> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("Start local TCP listen at {}", addr);
    let mut tunnel_id_seed: u32 = 0;
    while let Ok((inbound, _)) = listener.accept().await {
        let tunnel_id = tunnel_id_seed;
        tunnel_id_seed += 1;
        let tunnel_sender = sender.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_local_tunnel(inbound, tunnel_id, tunnel_sender).await {
                tracing::error!("handle local tunnel error:{}", e);
            }
        });
    }
    Ok(())
}
