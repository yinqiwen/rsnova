use crate::tunnel::Message;
use anyhow::{anyhow, Result};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::tunnel::tls_local;

async fn read_http_headers(inbound: &mut TcpStream) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let crlf2 = "\r\n\r\n".as_bytes();
    loop {
        let mut tmp_buf = [0; 4096];
        let n = inbound.read(&mut tmp_buf).await?;
        buf.extend_from_slice(&tmp_buf[0..n]);
        if let Some(_pos) = buf.windows(crlf2.len()).position(|window| window == crlf2) {
            return Ok(buf);
        }
    }
}

fn extract_target(headers_buf: &Vec<u8>, default_port: &str) -> Result<String> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    req.parse(headers_buf.as_slice())?;
    let mut target_addr: String = String::new();
    if let Some(path) = req.path {
        if path.starts_with("http://") {
            let url = url::Url::parse(path)?;
            if url.has_host() {
                target_addr.push_str(url.host_str().unwrap());
            }
            if let Some(p) = url.port() {
                target_addr.push(':');
                target_addr.push_str(p.to_string().as_str());
            }
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
    _tunnel_id: u32,
    mut inbound: TcpStream,
    sender: mpsc::UnboundedSender<Message>,
) -> Result<()> {
    let headers_buf = read_http_headers(&mut inbound).await?;
    let target_addr = extract_target(&headers_buf, ":80")?;

    tracing::info!("{}", target_addr);
    let msg = Message::open_stream(inbound, target_addr, Some(headers_buf));
    sender.send(msg)?;
    Ok(())
}

pub async fn handle_https(
    tunnel_id: u32,
    mut inbound: TcpStream,
    sender: mpsc::UnboundedSender<Message>,
) -> Result<()> {
    let headers_buf = read_http_headers(&mut inbound).await?;
    let conn_res = "HTTP/1.0 200 Connection established\r\n\r\n";
    inbound.write_all(conn_res.as_bytes()).await?;
    let target_addr = match tls_local::peek_sni(&mut inbound).await {
        Ok(mut sni) => {
            sni.push_str(":443");
            sni
        }
        Err(_) => extract_target(&headers_buf, ":443")?,
    };
    tracing::info!("[{}]Handle HTTPS proxy to {} ", tunnel_id, target_addr);
    let msg = Message::open_stream(inbound, target_addr, None);
    sender.send(msg)?;
    Ok(())
}
