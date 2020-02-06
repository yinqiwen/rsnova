use super::http::handle_http;
use super::http::handle_https;
use super::relay::relay_connection;
use super::rmux::handle_rmux;
use super::socks5::handle_socks5;
use super::tls::handle_tls;
use super::tls::valid_tls_version;
use super::ws::handle_websocket;
use crate::utils::{get_origin_dst, make_error};

use futures::FutureExt;
use std::env;
use std::error::Error;
use tokio::net::{TcpListener, TcpStream};

use std::sync::atomic::{AtomicU32, Ordering};
use url::Url;

use crate::config::TunnelConfig;

async fn handle_inbound(
    tunnel_id: u32,
    mut inbound: TcpStream,
    cfg: TunnelConfig,
) -> Result<(), Box<dyn Error>> {
    let mut peek_buf = [0u8; 3];
    inbound.peek(&mut peek_buf).await?;
    match peek_buf[0] {
        5 => {
            //socks5
            info!("[{}]Accept client as SOCKS5 proxy.", tunnel_id);
            handle_socks5(tunnel_id, inbound, &cfg).await?;
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
        handle_tls(tunnel_id, inbound, &cfg).await?;
        return Ok(());
    }
    if let Ok(prefix_str) = std::str::from_utf8(&peek_buf) {
        let prefix_str = prefix_str.to_uppercase();
        match prefix_str.as_str() {
            "GET" | "PUT" | "POS" | "DEL" | "OPT" | "TRA" | "PAT" | "HEA" | "CON" | "UPG" => {
                info!(
                    "[{}]Accept client as HTTP proxy with method:{}",
                    tunnel_id, prefix_str
                );
                //http proxy
                if prefix_str.as_str() == "CON" {
                    handle_https(tunnel_id, inbound, &cfg).await?;
                } else {
                    handle_http(tunnel_id, inbound, &cfg).await?;
                }
                return Ok(());
            }
            _ => {
                //nothing
            }
        };
    }
    if let Some(dst) = get_origin_dst(&inbound) {
        let target = format!("{}:{}", dst.ip().to_string(), dst.port());
        let relay = async move {
            let _ = relay_connection(tunnel_id, inbound, &cfg, target, Vec::new()).await;
        };
        tokio::spawn(relay);
        return Ok(());
    }
    Ok(())
}

pub async fn start_tunnel_server(mut cfg: TunnelConfig) -> Result<(), Box<dyn Error>> {
    let mut listen_str = String::from(cfg.listen.as_str());
    if cfg.listen.find("://").is_none() {
        listen_str.insert_str(0, "local://");
    }
    if listen_str.rfind(':') == listen_str.find(':') {
        let port = env::var("PORT").unwrap_or_else(|_| "3000".to_string());
        listen_str.push(':');
        listen_str.push_str(port.as_str());
    }

    for pac in cfg.pac.iter_mut() {
        pac.init();
    }

    let listen_url = match Url::parse(listen_str.as_str()) {
        Err(e) => {
            error!("invalid listen url:{} with error:{}", listen_str, e);
            return Err(make_error("invalid listen url"));
        }
        Ok(u) => u,
    };
    let addr = format!(
        "{}:{}",
        listen_url.host().unwrap(),
        listen_url.port().unwrap()
    );

    let mut listener = TcpListener::bind(addr).await?;
    let tunnel_id_seed = AtomicU32::new(0);
    while let Ok((inbound, _)) = listener.accept().await {
        let tunnel_id = tunnel_id_seed.fetch_add(1, Ordering::SeqCst);
        if listen_url.scheme() == "local" {
            let handle = handle_inbound(tunnel_id, inbound, cfg.clone()).map(move |r| {
                if let Err(e) = r {
                    error!("[{}]Failed to handle; error={}", tunnel_id, e);
                }
            });
            tokio::spawn(handle);
        } else if listen_url.scheme() == "rmux" {
            let handle = handle_rmux(tunnel_id, inbound, cfg.clone()).map(move |r| {
                if let Err(e) = r {
                    error!("[{}]Failed to handle; error={}", tunnel_id, e);
                }
            });
            tokio::spawn(handle);
        } else if listen_url.scheme() == "ws" {
            let handle = handle_websocket(tunnel_id, inbound, cfg.clone()).map(move |r| {
                if let Err(e) = r {
                    error!("[{}]Failed to handle; error={}", tunnel_id, e);
                }
            });
            tokio::spawn(handle);
        }
    }

    Ok(())
}
