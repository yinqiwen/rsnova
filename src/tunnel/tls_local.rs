use anyhow::{Result, anyhow};

use tokio::net::TcpStream;

use crate::tunnel::Message;
use crate::tunnel::client::ProxySender;

/// TLS record and handshake constants (RFC 5246, RFC 6066)
mod tls {
    pub const RECORD_TYPE_HANDSHAKE: u8 = 0x16;
    pub const HANDSHAKE_TYPE_CLIENT_HELLO: u8 = 0x01;
    pub const EXT_TYPE_SNI: u16 = 0x0000;
    pub const SNI_NAME_TYPE_HOSTNAME: u8 = 0x00;
}

/// A simple cursor for parsing binary data with bounds checking
struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn read_u8(&mut self) -> Result<u8> {
        if self.pos >= self.data.len() {
            return Err(anyhow!("unexpected end of data"));
        }
        let val = self.data[self.pos];
        self.pos += 1;
        Ok(val)
    }

    fn read_u16_be(&mut self) -> Result<u16> {
        if self.pos + 2 > self.data.len() {
            return Err(anyhow!("unexpected end of data"));
        }
        let val = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(val)
    }

    fn read_u24_be(&mut self) -> Result<u32> {
        if self.pos + 3 > self.data.len() {
            return Err(anyhow!("unexpected end of data"));
        }
        let val = ((self.data[self.pos] as u32) << 16)
            | ((self.data[self.pos + 1] as u32) << 8)
            | (self.data[self.pos + 2] as u32);
        self.pos += 3;
        Ok(val)
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        if self.pos + len > self.data.len() {
            return Err(anyhow!("unexpected end of data"));
        }
        let slice = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    fn skip(&mut self, len: usize) -> Result<()> {
        if self.pos + len > self.data.len() {
            return Err(anyhow!("unexpected end of data"));
        }
        self.pos += len;
        Ok(())
    }
}

/// Extract SNI hostname from TLS ClientHello message
///
/// TLS Record Layer (RFC 5246 Section 6.2.1):
/// ```text
/// struct {
///     ContentType type;           // 1 byte
///     ProtocolVersion version;    // 2 bytes
///     uint16 length;              // 2 bytes
///     opaque fragment[length];
/// } TLSPlaintext;
/// ```
///
/// Handshake Protocol (RFC 5246 Section 7.4):
/// ```text
/// struct {
///     HandshakeType msg_type;     // 1 byte
///     uint24 length;              // 3 bytes
///     ClientHello body;
/// } Handshake;
/// ```
///
/// ClientHello (RFC 5246 Section 7.4.1.2):
/// ```text
/// struct {
///     ProtocolVersion client_version;     // 2 bytes
///     Random random;                      // 32 bytes
///     SessionID session_id;               // 1 byte length + variable
///     CipherSuite cipher_suites<2..2^16-2>;
///     CompressionMethod compression_methods<1..2^8-1>;
///     Extension extensions<0..2^16-1>;    // optional
/// } ClientHello;
/// ```
///
/// SNI Extension (RFC 6066 Section 3):
/// ```text
/// struct {
///     NameType name_type;         // 1 byte
///     opaque HostName<1..2^16-1>; // 2 bytes length + hostname
/// } ServerName;
///
/// struct {
///     ServerName server_name_list<1..2^16-1>;
/// } ServerNameList;
/// ```
pub async fn peek_sni_v2(stream: &TcpStream) -> Result<String> {
    // Peek data from socket without consuming
    let mut buf = [0u8; 4096];
    let n = stream.peek(&mut buf).await?;
    if n == 0 {
        return Err(anyhow!("connection closed"));
    }
    let buf = &buf[..n];

    let mut p = Parser::new(buf);

    // --- TLS Record Layer ---
    let content_type = p.read_u8()?;
    if content_type != tls::RECORD_TYPE_HANDSHAKE {
        return Err(anyhow!("not a TLS handshake record"));
    }

    let _record_version = p.read_u16_be()?;
    let record_length = p.read_u16_be()? as usize;

    if p.remaining() < record_length {
        return Err(anyhow!("incomplete TLS record"));
    }

    // --- Handshake Protocol ---
    let handshake_type = p.read_u8()?;
    if handshake_type != tls::HANDSHAKE_TYPE_CLIENT_HELLO {
        return Err(anyhow!("not a ClientHello message"));
    }

    let handshake_length = p.read_u24_be()? as usize;
    if p.remaining() < handshake_length {
        return Err(anyhow!("incomplete ClientHello"));
    }

    // --- ClientHello ---
    let _client_version = p.read_u16_be()?;
    p.skip(32)?; // random

    // Session ID (variable length)
    let session_id_len = p.read_u8()? as usize;
    p.skip(session_id_len)?;

    // Cipher Suites (2 bytes length prefix)
    let cipher_suites_len = p.read_u16_be()? as usize;
    p.skip(cipher_suites_len)?;

    // Compression Methods (1 byte length prefix)
    let compression_len = p.read_u8()? as usize;
    p.skip(compression_len)?;

    // Extensions (optional, 2 bytes length prefix)
    if p.remaining() < 2 {
        return Err(anyhow!("no extensions present"));
    }
    let extensions_len = p.read_u16_be()? as usize;
    if p.remaining() < extensions_len {
        return Err(anyhow!("incomplete extensions data"));
    }

    // --- Parse Extensions ---
    let extensions_end = p.pos + extensions_len;
    while p.pos + 4 <= extensions_end {
        let ext_type = p.read_u16_be()?;
        let ext_len = p.read_u16_be()? as usize;

        if p.pos + ext_len > extensions_end {
            return Err(anyhow!("malformed extension"));
        }

        if ext_type == tls::EXT_TYPE_SNI {
            // --- SNI Extension ---
            let sni_list_len = p.read_u16_be()? as usize;
            let sni_list_end = p.pos + sni_list_len;

            while p.pos + 3 <= sni_list_end {
                let name_type = p.read_u8()?;
                let name_len = p.read_u16_be()? as usize;

                if p.pos + name_len > sni_list_end {
                    return Err(anyhow!("malformed server name"));
                }

                let name_bytes = p.read_bytes(name_len)?;

                if name_type == tls::SNI_NAME_TYPE_HOSTNAME {
                    let hostname = std::str::from_utf8(name_bytes)
                        .map_err(|_| anyhow!("invalid UTF-8 in SNI hostname"))?;
                    return Ok(hostname.to_string());
                }
            }
            return Err(anyhow!("no hostname in SNI extension"));
        }

        // Skip this extension
        p.skip(ext_len)?;
    }

    Err(anyhow!("SNI extension not found"))
}

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

pub async fn handle_tls(tunnel_id: u32, inbound: TcpStream, sender: ProxySender) -> Result<()> {
    let target_addr = match peek_sni_v2(&inbound).await {
        Ok(mut sni) => {
            sni.push_str(":443");
            sni
        }
        Err(_) => String::from(""),
    };
    if target_addr.is_empty() {
        tracing::error!("[{}]no sni found ", tunnel_id);
        super::transparent::handle_transparent(tunnel_id, inbound, sender).await
    } else {
        tracing::info!("[{}]Handle TLS proxy to {} ", tunnel_id, target_addr);
        let msg = Message::open_tcp_stream(inbound, target_addr, None);
        sender.send(msg).await?;
        Ok(())
    }
}
