use std::net::SocketAddr;
use std::sync::Arc;

use crate::tunnel::client::ProxySender;
use crate::tunnel::http_local::{handle_http, handle_https};
use crate::tunnel::socks5_local::handle_socks5;
use crate::tunnel::tls_local::{handle_tls, valid_tls_version};
use crate::utils::new_tcp_listener;
use anyhow::{Result, anyhow};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

const PROTOCOL_DETECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalProtocol {
    Socks5,
    Socks4,
    Tls,
    Http,
    HttpConnect,
    Transparent,
}

fn classify_prefix(prefix: &[u8]) -> Option<LocalProtocol> {
    if prefix.len() < 3 {
        return None;
    }
    match prefix[0] {
        5 => return Some(LocalProtocol::Socks5),
        4 => return Some(LocalProtocol::Socks4),
        _ => {}
    }
    if valid_tls_version(prefix) {
        return Some(LocalProtocol::Tls);
    }
    let method = [
        prefix[0].to_ascii_uppercase(),
        prefix[1].to_ascii_uppercase(),
        prefix[2].to_ascii_uppercase(),
    ];
    Some(match &method {
        b"CON" => LocalProtocol::HttpConnect,
        b"GET" | b"PUT" | b"POS" | b"DEL" | b"OPT" | b"TRA" | b"PAT" | b"HEA" | b"UPG" => {
            LocalProtocol::Http
        }
        _ => LocalProtocol::Transparent,
    })
}

async fn detect_protocol(inbound: &TcpStream) -> Result<LocalProtocol> {
    let deadline = tokio::time::Instant::now() + PROTOCOL_DETECT_TIMEOUT;
    let mut peek_buf = [0u8; 3];
    loop {
        let count = tokio::time::timeout_at(deadline, inbound.peek(&mut peek_buf))
            .await
            .map_err(|_| anyhow!("protocol detection timed out"))??;
        if count == 0 {
            return Err(anyhow!("connection closed during protocol detection"));
        }
        if let Some(protocol) = classify_prefix(&peek_buf[..count]) {
            return Ok(protocol);
        }
        tokio::time::timeout_at(
            deadline,
            tokio::time::sleep(std::time::Duration::from_millis(5)),
        )
        .await
        .map_err(|_| anyhow!("protocol detection timed out"))?;
    }
}

async fn handle_local_tunnel(
    inbound: TcpStream,
    tunnel_id: u32,
    sender: ProxySender,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<()> {
    match detect_protocol(&inbound).await? {
        LocalProtocol::Socks5 => {
            handle_socks5(tunnel_id, inbound, sender, direct_ctx).await?;
            Ok(())
        }
        LocalProtocol::Socks4 => {
            tracing::error!("socks4 not supported!");
            Err(anyhow!("socks4 unimplemented"))
        }
        LocalProtocol::Tls => handle_tls(tunnel_id, inbound, sender, direct_ctx).await,
        LocalProtocol::HttpConnect => handle_https(tunnel_id, inbound, sender, direct_ctx).await,
        LocalProtocol::Http => handle_http(tunnel_id, inbound, sender, direct_ctx).await,
        LocalProtocol::Transparent => {
            tracing::info!(
                "[{}]Accept client with non socks5/tls/http traffic.",
                tunnel_id
            );
            super::transparent::handle_transparent(tunnel_id, inbound, sender, direct_ctx).await
        }
    }
}

pub async fn start_local_tunnel_server(
    addr: &SocketAddr,
    sender: ProxySender,
    tproxy: bool,
    max_connections: usize,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<(), std::io::Error> {
    let listener = new_tcp_listener(addr, tproxy).await?;
    let semaphore = Arc::new(Semaphore::new(max_connections));

    #[cfg(target_os = "linux")]
    {
        use crate::tunnel::udp_local::start_local_udp_tunnel_server;
        if tproxy {
            start_local_udp_tunnel_server(addr, sender.clone())?;
        }
    }

    tracing::info!(
        "Start local TCP listen at {} (max connections: {})",
        addr,
        max_connections
    );
    let mut tunnel_id_seed: u32 = 0;
    let mut accept_backoff = crate::utils::AcceptBackoff::default();
    loop {
        let (inbound, _) = match listener.accept().await {
            Ok(accepted) => {
                accept_backoff.reset();
                accepted
            }
            Err(error) => {
                let delay = accept_backoff.next_delay();
                metrics::counter!("local_accept_retries").increment(1);
                tracing::warn!("local accept failed: {}; retrying in {:?}", error, delay);
                tokio::time::sleep(delay).await;
                continue;
            }
        };
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(
                    "Max connections ({}) reached, rejecting new connection",
                    max_connections
                );
                continue;
            }
        };
        let tunnel_id = tunnel_id_seed;
        tunnel_id_seed += 1;
        let tunnel_sender = sender.clone();
        let direct_ctx = direct_ctx.clone();
        tokio::spawn(async move {
            let _permit = permit; // hold permit until task completes
            if let Err(e) = handle_local_tunnel(inbound, tunnel_id, tunnel_sender, direct_ctx).await
            {
                tracing::error!("handle local tunnel error:{}", e);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    async fn fragmented_protocol(parts: &[&[u8]]) -> LocalProtocol {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
        let (server, _) = listener.accept().await.unwrap();
        let mut client = client.await.unwrap();
        let parts: Vec<Vec<u8>> = parts.iter().map(|part| part.to_vec()).collect();
        let writer = tokio::spawn(async move {
            for part in parts {
                client.write_all(&part).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
        });
        let protocol = detect_protocol(&server).await.unwrap();
        writer.await.unwrap();
        protocol
    }

    #[test]
    fn partial_prefix_is_not_classified() {
        assert_eq!(classify_prefix(b"G"), None);
        assert_eq!(classify_prefix(&[0x16, 0x03]), None);
    }

    #[tokio::test]
    async fn fragmented_http_prefix_is_detected_as_http() {
        assert_eq!(
            fragmented_protocol(&[b"G", b"ET"]).await,
            LocalProtocol::Http
        );
    }

    #[tokio::test]
    async fn fragmented_tls_prefix_is_detected_as_tls() {
        assert_eq!(
            fragmented_protocol(&[&[0x16], &[0x03, 0x01]]).await,
            LocalProtocol::Tls
        );
    }
}
