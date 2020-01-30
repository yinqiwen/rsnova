use super::relay::relay_connection;
use crate::utils::make_error;

use std::error::Error;

use tokio::io::AsyncReadExt;

use tokio::net::TcpStream;

use crate::config::TunnelConfig;

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

pub async fn peek_sni(inbound: &mut TcpStream) -> Result<(String, Vec<u8>), Box<dyn Error>> {
    let mut peek_buf = Vec::new();
    let mut ver_len_buf = [0u8; 5];
    inbound.read_exact(&mut ver_len_buf).await?;
    peek_buf.extend_from_slice(&ver_len_buf);
    let mut n = ver_len_buf[3] as u16;
    n = (n << 8) + ver_len_buf[4] as u16;
    if n < 42 {
        return Err(make_error("no sufficient space for sni"));
    }
    let mut vdata = vec![0; n as usize];
    inbound.read_exact(&mut vdata).await?;
    peek_buf.extend_from_slice(&vdata[..]);
    if vdata[0] != 0x01 {
        return Err(make_error("not clienthello handshake"));
    }
    let rest_buf = &vdata[38..];
    let sid_len = rest_buf[0] as usize;
    let rest_buf = &rest_buf[(1 + sid_len)..];
    if rest_buf.len() < 2 {
        return Err(make_error("no sufficient space for sni0"));
    }
    let mut cipher_len = rest_buf[0] as usize;
    cipher_len = (cipher_len << 8) + rest_buf[1] as usize;
    if cipher_len % 2 == 1 || rest_buf.len() < 3 + cipher_len {
        return Err(make_error("invalid cipher_len"));
    }
    let rest_buf = &rest_buf[(2 + cipher_len)..];
    let compress_method_len = rest_buf[0] as usize;
    if rest_buf.len() < 1 + compress_method_len {
        return Err(make_error("invalid compress_method_len"));
    }
    let rest_buf = &rest_buf[(1 + compress_method_len)..];
    if rest_buf.len() < 2 {
        return Err(make_error("invalid after compress_method"));
    }
    let mut ext_len = rest_buf[0] as usize;
    ext_len = (ext_len << 8) + rest_buf[1] as usize;
    let rest_buf = &rest_buf[2..];
    if rest_buf.len() < ext_len {
        return Err(make_error("invalid ext_len"));
    }
    if ext_len == 0 {
        return Err(make_error("no extension in client_hello"));
    }
    let mut ext_buf = rest_buf;
    loop {
        if ext_buf.len() < 4 {
            return Err(make_error("invalid ext buf len"));
        }
        let mut extension = ext_buf[0] as usize;
        extension = (extension << 8) + ext_buf[1] as usize;
        let mut length = ext_buf[2] as usize;
        length = (length << 8) + ext_buf[3] as usize;
        ext_buf = &ext_buf[4..];
        if ext_buf.len() < length {
            return Err(make_error("invalid ext buf content"));
        }
        if extension == 0 {
            if length < 2 {
                return Err(make_error("invalid ext buf length"));
            }
            let mut num_names = ext_buf[0] as usize;
            num_names = (num_names << 8) + ext_buf[1] as usize;
            let mut data = &ext_buf[2..];
            for _ in 0..num_names {
                if data.len() < 3 {
                    return Err(make_error("invalid ext data length"));
                }
                let name_type = data[0];
                let mut name_len = data[1] as usize;
                name_len = (name_len << 8) + data[2] as usize;
                data = &data[3..];
                if data.len() < name_len {
                    return Err(make_error("invalid ext name data"));
                }
                if name_type == 0 {
                    let server_name = String::from_utf8_lossy(&data[0..name_len]);
                    debug!("####Peek SNI:{}", server_name);
                    return Ok((String::from(server_name), peek_buf));
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
    cfg: &TunnelConfig,
) -> Result<(), Box<dyn Error>> {
    let (sni, peek_buf) = peek_sni(&mut inbound).await?;
    let mut target = sni;
    target.push_str(":443");

    info!("[{}]Handle TLS proxy to {}", tunnel_id, target);
    relay_connection(tunnel_id, inbound, cfg, target, peek_buf).await?;
    Ok(())
}
