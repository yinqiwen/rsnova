use anyhow::{anyhow, Result};

use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::tunnel::Message;

pub fn valid_tls_version(buf: &[u8]) -> bool {
    if buf.len() < 3 {
        return false;
    }
    //recordTypeHandshake
    if buf[0] != 0x16 {
        //info!("###1 here {}", buf[0]);
        return false;
    }
    let tls_major_ver = buf[1];
    //let tlsMinorVer = buf[2];

    if tls_major_ver < 3 {
        //no SNI before sslv3
        //info!("###2 here {}", tls_major_ver);
        return false;
    }
    true
}

pub async fn peek_sni(inbound: &mut TcpStream) -> Result<String> {
    let mut peek_buf: Vec<u8> = Vec::new();
    peek_buf.resize(4096, 0);
    let ver_len: usize = 5;
    let mut peek_cursor: usize = 0;
    let peek_n = inbound.peek(peek_buf.as_mut_slice()).await?;
    if peek_n < ver_len {
        return Err(anyhow!("no sufficient peek space for sni"));
    }
    let mut n = peek_buf[3] as u16;
    n = (n << 8) + peek_buf[4] as u16;
    if n < 42 {
        return Err(anyhow!("no sufficient space for sni"));
    }
    peek_cursor += ver_len;
    if peek_n < (peek_cursor + n as usize) {
        return Err(anyhow!("no sufficient peek buffer space for sni"));
    }
    if peek_buf[peek_cursor] != 0x01 {
        return Err(anyhow!("not clienthello handshake"));
    }
    let rest_buf = &peek_buf.as_slice()[peek_cursor + 38..];
    let sid_len = rest_buf[0] as usize;
    let rest_buf = &rest_buf[(1 + sid_len)..];
    if rest_buf.len() < 2 {
        return Err(anyhow!("no sufficient space for sni"));
    }
    let mut cipher_len = rest_buf[0] as usize;
    cipher_len = (cipher_len << 8) + rest_buf[1] as usize;
    if cipher_len % 2 == 1 || rest_buf.len() < 3 + cipher_len {
        return Err(anyhow!("invalid cipher_len"));
    }
    let rest_buf = &rest_buf[(2 + cipher_len)..];
    let compress_method_len = rest_buf[0] as usize;
    if rest_buf.len() < 1 + compress_method_len {
        return Err(anyhow!("invalid compress_method_len"));
    }
    let rest_buf = &rest_buf[(1 + compress_method_len)..];
    if rest_buf.len() < 2 {
        return Err(anyhow!("invalid after compress_method"));
    }
    let mut ext_len = rest_buf[0] as usize;
    ext_len = (ext_len << 8) + rest_buf[1] as usize;
    let rest_buf = &rest_buf[2..];
    if rest_buf.len() < ext_len {
        return Err(anyhow!("invalid ext_len"));
    }
    if ext_len == 0 {
        return Err(anyhow!("no extension in client_hello"));
    }
    let mut ext_buf = rest_buf;
    loop {
        if ext_buf.len() < 4 {
            return Err(anyhow!("invalid ext buf len"));
        }
        let mut extension = ext_buf[0] as usize;
        extension = (extension << 8) + ext_buf[1] as usize;
        let mut length = ext_buf[2] as usize;
        length = (length << 8) + ext_buf[3] as usize;
        ext_buf = &ext_buf[4..];
        if ext_buf.len() < length {
            return Err(anyhow!("invalid ext buf content"));
        }
        if extension == 0 {
            if length < 2 {
                return Err(anyhow!("invalid ext buf length"));
            }
            let mut num_names = ext_buf[0] as usize;
            num_names = (num_names << 8) + ext_buf[1] as usize;
            let mut data = &ext_buf[2..];
            for _ in 0..num_names {
                if data.len() < 3 {
                    return Err(anyhow!("invalid ext data length"));
                }
                let name_type = data[0];
                let mut name_len = data[1] as usize;
                name_len = (name_len << 8) + data[2] as usize;
                data = &data[3..];
                if data.len() < name_len {
                    return Err(anyhow!("invalid ext name data"));
                }
                if name_type == 0 {
                    let server_name = String::from_utf8_lossy(&data[0..name_len]);
                    tracing::info!("Peek SNI:{}", server_name);
                    return Ok(String::from(server_name));
                }
                data = &data[name_len..];
            }
        }
        ext_buf = &ext_buf[length..];
    }
}

pub async fn handle_tls(
    tunnel_id: u32,
    mut inbound: TcpStream,
    sender: mpsc::UnboundedSender<Message>,
) -> Result<()> {
    let target_addr = match peek_sni(&mut inbound).await {
        Ok(mut sni) => {
            sni.push_str(":443");
            sni
        }
        Err(_) => String::from(""),
    };
    if target_addr.is_empty() {
        return Err(anyhow!("no sni found"));
    }
    tracing::info!("[{}]Handle TLS proxy to {} ", tunnel_id, target_addr);
    let msg = Message::open_stream(inbound, target_addr, None);
    sender.send(msg)?;
    Ok(())
}
