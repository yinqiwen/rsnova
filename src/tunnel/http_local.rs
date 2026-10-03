use crate::tunnel::Message;
use crate::tunnel::client::{ConnectReply, ProxySender};
use anyhow::{Result, anyhow};

use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};

const MAX_HTTP_HEADER_SIZE: usize = 64 * 1024; // 64KB
const HTTP_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

struct HttpHead {
    headers: Vec<u8>,
    tail: Vec<u8>,
}

fn split_http_headers(buf: &[u8]) -> Option<HttpHead> {
    let end = buf.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    Some(HttpHead {
        headers: buf[..end].to_vec(),
        tail: buf[end..].to_vec(),
    })
}

async fn read_http_headers(inbound: &mut TcpStream) -> Result<HttpHead> {
    // Pre-allocate to avoid reallocation on the first few extends.
    // Most HTTP request headers fit in 1-2 KB.
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let deadline = Instant::now() + HTTP_HEADER_READ_TIMEOUT;
    loop {
        let mut tmp_buf = [0; 4096];
        let n = match timeout_at(deadline, inbound.read(&mut tmp_buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err(anyhow!("read http header timeout")),
        };
        if n == 0 {
            return Err(anyhow!("connection closed before headers complete"));
        }
        buf.extend_from_slice(&tmp_buf[0..n]);
        if buf.len() > MAX_HTTP_HEADER_SIZE {
            return Err(anyhow!("http header too large"));
        }
        if let Some(head) = split_http_headers(&buf) {
            return Ok(head);
        }
    }
}

fn format_host_port(host: &str, port: u16) -> String {
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => std::net::SocketAddr::new(ip, port).to_string(),
        Err(_) => format!("{host}:{port}"),
    }
}

fn normalize_authority(authority: &str, default_port: u16) -> Result<String> {
    let authority = authority.trim();
    if let Ok(addr) = authority.parse::<std::net::SocketAddr>() {
        return Ok(addr.to_string());
    }
    if let Ok(ip) = authority.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(ip, default_port).to_string());
    }
    if authority.starts_with('[') {
        if let Some(end) = authority.find(']') {
            let host = &authority[1..end];
            let port = authority[end + 1..]
                .strip_prefix(':')
                .map(str::parse)
                .transpose()?
                .unwrap_or(default_port);
            return Ok(format_host_port(host, port));
        }
        return Err(anyhow!("invalid IPv6 authority"));
    }
    if let Some((host, port)) = authority.rsplit_once(':')
        && !host.contains(':')
        && let Ok(port) = port.parse::<u16>()
    {
        return Ok(format_host_port(host, port));
    }
    Ok(format_host_port(authority, default_port))
}

fn extract_target(headers_buf: &[u8], default_port: u16) -> Result<String> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    req.parse(headers_buf)?;
    if req.method == Some("CONNECT")
        && let Some(authority) = req.path
    {
        return normalize_authority(authority, default_port);
    }
    if let Some(path) = req.path
        && (path.starts_with("http://") || path.starts_with("https://"))
    {
        let url = url::Url::parse(path)?;
        let host = url.host_str().ok_or_else(|| anyhow!("URL has no host"))?;
        return Ok(format_host_port(
            host,
            url.port_or_known_default().unwrap_or(default_port),
        ));
    }
    for h in req.headers {
        if h.name.eq_ignore_ascii_case("Host") {
            return normalize_authority(std::str::from_utf8(h.value)?, default_port);
        }
    }
    Err(anyhow!("Can not get target addr."))
}

pub async fn handle_http(
    tunnel_id: u32,
    mut inbound: TcpStream,
    sender: ProxySender,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<()> {
    let head = read_http_headers(&mut inbound).await?;
    let target_addr = extract_target(&head.headers, 80)?;
    let mut payload = head.headers;
    payload.extend_from_slice(&head.tail);
    // let original_dst = crate::utils::get_original_dst(&inbound)
    //     .map(|a| a.to_string())
    //     .unwrap_or_else(|_| "N/A".to_string());
    // let headers_str = String::from_utf8_lossy(&headers_buf);
    // let src = inbound.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "N/A".to_string());
    // tracing::info!("[{tunnel_id}] HTTP src={src} target={target_addr} original_dst={original_dst} headers={headers_str}");
    tracing::info!("[{}]Handle HTTP proxy to {} ", tunnel_id, target_addr);
    if direct_ctx
        .try_bypass(
            tunnel_id,
            &mut inbound,
            &target_addr,
            Some(&payload),
            ConnectReply::None,
        )
        .await?
    {
        return Ok(());
    }
    let msg = Message::open_tcp_stream(inbound, target_addr, Some(payload), ConnectReply::None);
    sender.send(msg).await?;
    Ok(())
}

pub async fn handle_https(
    tunnel_id: u32,
    mut inbound: TcpStream,
    sender: ProxySender,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<()> {
    let head = read_http_headers(&mut inbound).await?;
    let target_addr = extract_target(&head.headers, 443)?;
    tracing::info!("[{}]Handle HTTPS proxy to {} ", tunnel_id, target_addr);
    if direct_ctx
        .try_bypass(
            tunnel_id,
            &mut inbound,
            &target_addr,
            Some(&head.tail),
            ConnectReply::HttpConnect,
        )
        .await?
    {
        return Ok(());
    }
    let payload = (!head.tail.is_empty()).then_some(head.tail);
    let msg = Message::open_tcp_stream(inbound, target_addr, payload, ConnectReply::HttpConnect);
    sender.send(msg).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_headers_preserve_coalesced_payload() {
        let bytes =
            b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\nclient-hello";
        let parsed = split_http_headers(bytes).unwrap();
        assert_eq!(parsed.headers.last(), Some(&b'\n'));
        assert_eq!(parsed.tail, b"client-hello");
    }

    #[test]
    fn ipv6_authority_is_bracketed() {
        assert_eq!(
            normalize_authority("2001:db8::1", 443).unwrap(),
            "[2001:db8::1]:443"
        );
        assert_eq!(
            normalize_authority("[2001:db8::1]", 80).unwrap(),
            "[2001:db8::1]:80"
        );
    }

    #[test]
    fn connect_uses_request_authority() {
        let bytes = b"CONNECT [2001:db8::2]:8443 HTTP/1.1\r\nHost: ignored.example\r\n\r\n";
        assert_eq!(extract_target(bytes, 443).unwrap(), "[2001:db8::2]:8443");
    }
}
