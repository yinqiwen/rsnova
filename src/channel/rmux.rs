use super::ChannelStream;
use crate::config::ChannelConfig;

use crate::rmux::{
    create_stream, handle_rmux_session, new_auth_event, read_encrypt_event, write_encrypt_event,
    AuthRequest, AuthResponse, CryptoContext,
};
use crate::utils::make_io_error;
use bytes::BytesMut;
use std::io::{Error, ErrorKind};
use tokio::net::TcpStream;

pub async fn init_rmux_client(
    config: ChannelConfig,
    session_id: u32,
) -> Result<(), std::io::Error> {
    let conn = TcpStream::connect(&config.url);
    let dur = std::time::Duration::from_secs(3);
    info!("connect rmux:{}", config.url);
    let s = tokio::time::timeout(dur, conn).await?;
    match s {
        Err(e) => Err(e),
        Ok(mut c) => {
            let sid = 0 as u32;
            let auth = AuthRequest {
                //key: String::from(key),
                method: String::from(config.cipher.method.as_str()),
            };
            let ev = new_auth_event(sid, &auth);
            let key = String::from(config.cipher.key.as_str());
            let method = String::from(config.cipher.method.as_str());
            let mut rctx = CryptoContext::new(method.as_str(), key.as_str(), 0);
            let mut wctx = CryptoContext::new(method.as_str(), key.as_str(), 0);
            write_encrypt_event(&mut wctx, &mut c, ev).await?;
            let mut recv_buf = BytesMut::new();
            let recv_ev = match read_encrypt_event(&mut rctx, &mut c, &mut recv_buf).await {
                Err(_) => return Err(make_io_error("can NOT read first auth envent.")),
                Ok(None) => return Err(make_io_error("can NOT read first auth envent.")),
                Ok(Some(ev)) => ev,
            };
            let decoded: AuthResponse = bincode::deserialize(&recv_ev.body[..]).unwrap();
            if !decoded.success {
                let _ = c.shutdown(std::net::Shutdown::Both);
                return Err(Error::from(ErrorKind::ConnectionRefused));
            }
            let rctx = CryptoContext::new(method.as_str(), key.as_str(), decoded.rand);
            let wctx = CryptoContext::new(method.as_str(), key.as_str(), decoded.rand);
            handle_rmux_session(
                config.name.as_str(),
                session_id,
                c,
                rctx,
                wctx,
                &mut recv_buf,
                config.max_alive_mins as u64 * 60,
            )
            .await?;
            Ok(())
        }
    }
}

pub async fn get_rmux_stream(
    channel: &str,
    addr: String,
) -> Result<Box<dyn ChannelStream + Send>, std::io::Error> {
    let stream = create_stream(channel, "tcp", addr.as_str()).await?;
    Ok(Box::new(stream))
}

// pub async fn init_rmux_client(config: ChannelConfig) -> Result<(), std::io::Error> {
//     // let conn = TcpStream::connect(&addr);
//     // let dur = std::time::Duration::from_secs(3);
//     // let s = tokio::time::timeout(dur, conn).await?;
//     // match s {
//     //     Err(e) => Err(e),
//     //     Ok(c) => {
//     //         //
//     //         Ok(())
//     //     }
//     // }

//     Ok(())
// }
