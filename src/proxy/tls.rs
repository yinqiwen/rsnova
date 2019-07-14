use crate::common::{other_error, peek_exact2, PeekableReader};
use crate::mux::mux_relay_connection;
use crate::proxy::local::*;

use tokio_io::{AsyncRead, AsyncWrite};

use futures::Future;

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
    return true;
}

pub fn peek_sni<R>(
    reader: PeekableReader<R>,
) -> impl Future<Item = (PeekableReader<R>, String), Error = (PeekableReader<R>, std::io::Error)>
where
    R: AsyncRead + Send + 'static,
{
    let peek_ver_len = peek_exact2(reader, [0u8; 5]).and_then(move |(reader, data)| {
        let mut n = data[3] as u16;
        n = (n << 8) + data[4] as u16;
        if n < 47 {
            Err((reader, other_error("no sufficient space for sni")))
        } else {
            Ok((reader, n + 5))
        }
    });

    peek_ver_len.and_then(|(_reader, n)| {
        let vdata = vec![0; n as usize];
        peek_exact2(_reader, vdata).and_then(|(_reader, data)| {
            //tlsHandshakeTypeClientHello
            if data[5] != 0x01 || data.len() < 47 {
                Err((_reader, other_error("not clienthello handshake")))
            } else {
                let rest_buf = &data[43..];
                let sid_len = rest_buf[0] as usize;
                let rest_buf = &rest_buf[(1 + sid_len)..];
                if rest_buf.len() < 2 {
                    return Err((_reader, other_error("no space to get sni0")));
                }
                let mut cipher_len = rest_buf[0] as usize;
                cipher_len = (cipher_len << 8) + rest_buf[1] as usize;
                if cipher_len % 2 == 1 || rest_buf.len() < 3 + cipher_len {
                    return Err((_reader, other_error("invalid cipher_len")));
                }
                let rest_buf = &rest_buf[(2 + cipher_len)..];
                let compress_method_len = rest_buf[0] as usize;
                if rest_buf.len() < 1 + compress_method_len {
                    return Err((_reader, other_error("invalid compress_method_len")));
                }
                let rest_buf = &rest_buf[(1 + compress_method_len)..];
                if rest_buf.len() < 2 {
                    return Err((_reader, other_error("invalid after compress_method")));
                }
                let mut ext_len = rest_buf[0] as usize;
                ext_len = (ext_len << 8) + rest_buf[1] as usize;
                let rest_buf = &rest_buf[2..];
                if rest_buf.len() < ext_len {
                    return Err((_reader, other_error("invalid ext_len")));
                }
                if ext_len == 0 {
                    return Err((_reader, other_error("no extension in client_hello")));
                }
                let mut ext_buf = rest_buf;
                loop {
                    if ext_buf.len() < 4 {
                        return Err((_reader, other_error("invalid ext buf len")));
                    }
                    let mut extension = ext_buf[0] as usize;
                    extension = (extension << 8) + ext_buf[1] as usize;
                    let mut length = ext_buf[2] as usize;
                    length = (length << 8) + ext_buf[3] as usize;
                    ext_buf = &ext_buf[4..];
                    if ext_buf.len() < length {
                        return Err((_reader, other_error("invalid ext buf content")));
                    }
                    if extension == 0 {
                        if length < 2 {
                            return Err((_reader, other_error("invalid ext buf length")));
                        }
                        let mut num_names = ext_buf[0] as usize;
                        num_names = (num_names << 8) + ext_buf[1] as usize;
                        let mut data = &ext_buf[2..];
                        for _ in 0..num_names {
                            if data.len() < 3 {
                                return Err((_reader, other_error("invalid ext data length")));
                            }
                            let name_type = data[0];
                            let mut name_len = data[1] as usize;
                            name_len = (name_len << 8) + data[2] as usize;
                            data = &data[3..];
                            if data.len() < name_len {
                                return Err((_reader, other_error("invalid ext name data")));
                            }
                            if name_type == 0 {
                                let server_name = String::from_utf8_lossy(&data[0..name_len]);
                                debug!("####Peek SNI:{}", server_name);
                                return Ok((_reader, String::from(server_name)));
                            }
                            data = &data[name_len..];
                        }
                    }
                    ext_buf = &ext_buf[length..];
                }
            }
        })
    })
}

pub fn handle_tls_connection<R, W>(
    ctx: LocalContext,
    reader: PeekableReader<R>,
    writer: W,
) -> impl Future<Item = (), Error = ()>
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    peek_sni(reader)
        .map_err(|(_r, e)| {
            error!("Failed to peek sni for reason:{}", e);
        })
        .and_then(|(_reader, sni)| {
            let mut target = sni;
            target.push_str(":443");
            mux_relay_connection(_reader, writer, "tcp", target.as_str(), 30, None, true)
        })
}
