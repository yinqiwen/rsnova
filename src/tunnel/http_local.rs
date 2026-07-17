use crate::tunnel::Message;
use crate::tunnel::client::ProxySender;
use anyhow::{Result, anyhow};

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::tunnel::tls_local;

const MAX_HTTP_HEADER_SIZE: usize = 64 * 1024; // 64KB
const HTTP_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

async fn read_http_headers(inbound: &mut TcpStream) -> Result<Vec<u8>> {
    // Pre-allocate to avoid reallocation on the first few extends.
    // Most HTTP request headers fit in 1-2 KB.
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let crlf2: &[u8] = b"\r\n\r\n";
    // Scan only newly-appended bytes for `\r\n\r\n`. We track the index of the
    // last byte we've already considered so we never re-scan the whole buffer
    // (the previous `windows().position()` was O(n²) under slowloris-style
    // 1-byte-at-a-time sends).
    let mut scan_from: usize = 0;
    loop {
        let mut tmp_buf = [0; 4096];
        let n = match timeout(HTTP_HEADER_READ_TIMEOUT, inbound.read(&mut tmp_buf)).await {
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
        // Search for `\r\n\r\n` only in the tail we haven't scanned. Because the
        // pattern can span the boundary, back up by `crlf2.len() - 1` bytes.
        let start = scan_from.saturating_sub(crlf2.len() - 1);
        if let Some(pos) = buf[start..]
            .windows(crlf2.len())
            .position(|window| window == crlf2)
        {
            let _absolute_pos = start + pos;
            return Ok(buf);
        }
        scan_from = buf.len();
    }
}

fn extract_target(headers_buf: &Vec<u8>, default_port: &str) -> Result<String> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    req.parse(headers_buf.as_slice())?;
    let mut target_addr: String = String::new();
    if let Some(path) = req.path
        && path.starts_with("http://")
    {
        let url = url::Url::parse(path)?;
        if url.has_host() {
            target_addr.push_str(url.host_str().unwrap());
        }
        if let Some(p) = url.port() {
            target_addr.push(':');
            target_addr.push_str(p.to_string().as_str());
        }
    }
    if target_addr.is_empty() {
        for h in req.headers {
            if h.name.eq_ignore_ascii_case("Host") {
                target_addr = String::from(std::str::from_utf8(h.value)?);
                break;
            }
        }
    }
    if target_addr.is_empty() {
        return Err(anyhow!("Can not get target addr."));
    }
    if target_addr.find(':').is_none() {
        target_addr.push_str(default_port);
    }
    Ok(target_addr)
}

pub async fn handle_http(
    tunnel_id: u32,
    mut inbound: TcpStream,
    sender: ProxySender,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<()> {
    let headers_buf = read_http_headers(&mut inbound).await?;
    let target_addr = extract_target(&headers_buf, ":80")?;
    // let original_dst = crate::utils::get_original_dst(&inbound)
    //     .map(|a| a.to_string())
    //     .unwrap_or_else(|_| "N/A".to_string());
    // let headers_str = String::from_utf8_lossy(&headers_buf);
    // let src = inbound.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "N/A".to_string());
    // tracing::info!("[{tunnel_id}] HTTP src={src} target={target_addr} original_dst={original_dst} headers={headers_str}");
    tracing::info!("[{}]Handle HTTP proxy to {} ", tunnel_id, target_addr);
    if direct_ctx
        .try_bypass(tunnel_id, &mut inbound, &target_addr, Some(&headers_buf))
        .await?
    {
        return Ok(());
    }
    let msg = Message::open_tcp_stream(inbound, target_addr, Some(headers_buf));
    sender.send(msg).await?;
    Ok(())
}

pub async fn handle_https(
    tunnel_id: u32,
    mut inbound: TcpStream,
    sender: ProxySender,
    direct_ctx: crate::tunnel::direct::DirectCtx,
) -> Result<()> {
    let headers_buf = read_http_headers(&mut inbound).await?;
    let conn_res = "HTTP/1.0 200 Connection established\r\n\r\n";
    inbound.write_all(conn_res.as_bytes()).await?;
    let target_addr = match tls_local::peek_sni_v2(&inbound).await {
        Ok(mut sni) => {
            sni.push_str(":443");
            sni
        }
        Err(_) => extract_target(&headers_buf, ":443")?,
    };
    tracing::info!("[{}]Handle HTTPS proxy to {} ", tunnel_id, target_addr);
    if direct_ctx
        .try_bypass(tunnel_id, &mut inbound, &target_addr, None)
        .await?
    {
        return Ok(());
    }
    let msg = Message::open_tcp_stream(inbound, target_addr, None);
    sender.send(msg).await?;
    Ok(())
}
