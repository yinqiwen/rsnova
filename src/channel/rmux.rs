use super::ChannelStream;
use crate::config::ChannelConfig;

use crate::rmux::{
    create_stream, new_auth_event, process_rmux_session, read_encrypt_event, write_encrypt_event,
    AuthRequest, AuthResponse, CryptoContext, MuxContext,
};
use crate::utils::{make_io_error, AsyncTcpStream, AsyncTokioIO, WebsocketReader, WebsocketWriter};
//use crate::utils::make_io_error;
use async_tls::TlsConnector;
use bytes::BytesMut;
use futures::StreamExt;
use std::error::Error;
use std::io::ErrorKind;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use url::Url;

async fn init_client<'a, R, W>(
    config: ChannelConfig,
    session_id: u32,
    ri: &'a mut R,
    wi: &'a mut W,
) -> Result<(), std::io::Error>
where
    R: AsyncRead + Unpin + Sized,
    W: AsyncWrite + Unpin + Sized,
{
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
    write_encrypt_event(&mut wctx, wi, ev).await?;
    let mut recv_buf = BytesMut::new();
    let recv_ev = match read_encrypt_event(&mut rctx, ri, &mut recv_buf).await {
        Err(e) => return Err(make_io_error(e.description())),
        Ok(None) => return Err(make_io_error("can NOT read first auth envent.")),
        Ok(Some(ev)) => ev,
    };
    let decoded: AuthResponse = bincode::deserialize(&recv_ev.body[..]).unwrap();
    if !decoded.success {
        //let _ = c.shutdown(std::net::Shutdown::Both);
        return Err(std::io::Error::from(ErrorKind::ConnectionRefused));
    }
    let rctx = CryptoContext::new(method.as_str(), key.as_str(), decoded.rand);
    let wctx = CryptoContext::new(method.as_str(), key.as_str(), decoded.rand);
    let ctx = MuxContext::new(
        config.name.as_str(),
        session_id,
        rctx,
        wctx,
        config.max_alive_mins as u64 * 60,
        &mut recv_buf,
    );
    process_rmux_session(
        ctx, // config.name.as_str(),
        // session_id,
        ri,
        wi,
        // rctx,
        // wctx,
        // &mut recv_buf,
        // config.max_alive_mins as u64 * 60,
    )
    .await?;
    Ok(())
}

pub async fn init_rmux_client(
    config: ChannelConfig,
    session_id: u32,
) -> Result<(), std::io::Error> {
    let mut url = String::from(config.url.as_str());
    if config.url.find("://").is_none() {
        url = String::from("rmux://");
        url.push_str(config.url.as_str());
    }

    let conn_url = match Url::parse(url.as_str()) {
        Err(e) => {
            error!("invalid connect url:{} with error:{}", url, e);
            return Err(make_io_error("invalid connect url"));
        }
        Ok(u) => u,
    };
    let addr = if config.sni_proxy.is_some() {
        let mut v = String::from(config.sni_proxy.as_ref().unwrap());
        if v.find(':').is_none() {
            v.push_str(":443");
        }
        v
    } else {
        format!(
            "{}:{}",
            conn_url.host().as_ref().unwrap(),
            conn_url.port_or_known_default().unwrap()
        )
    };
    info!("connect rmux:{} to addr:{}", url, addr);

    let domain = if config.sni.is_some() {
        config.sni.as_ref().unwrap().as_str()
    } else {
        conn_url.host_str().unwrap()
    };

    let conn = TcpStream::connect(&addr);
    let dur = std::time::Duration::from_secs(5);
    let s = tokio::time::timeout(dur, conn).await?;
    let mut conn = match s {
        Err(e) => return Err(e),
        Ok(c) => c,
    };
    match conn_url.scheme() {
        "rmux" => {
            let (mut read, mut write) = conn.split();
            let rc = init_client(config, session_id, &mut read, &mut write).await;
            let _ = conn.shutdown(std::net::Shutdown::Both);
            if rc.is_err() {
                return rc;
            }
        }
        "ws" => {
            let ws = match tokio_tungstenite::client_async(url, conn).await {
                Err(e) => return Err(make_io_error(e.description())),
                Ok((s, _)) => s,
            };
            let (write, read) = ws.split();
            let mut reader = WebsocketReader::new(read);
            let mut writer = WebsocketWriter::new(write);
            let rc = init_client(config, session_id, &mut reader, &mut writer).await;
            writer.shutdown().await?;
            if rc.is_err() {
                return rc;
            }
        }
        "wss" => {
            let connector = TlsConnector::default();
            let conn = AsyncTcpStream::new(conn);
            //let host = conn_url.host_str();
            info!("TLS connect {:?}", domain);
            let tls_stream = connector.connect(domain, conn)?.await?;
            let conn = AsyncTokioIO::new(tls_stream);
            let ws = match tokio_tungstenite::client_async(url, conn).await {
                Err(e) => return Err(make_io_error(e.description())),
                Ok((s, _)) => s,
            };
            let (write, read) = ws.split();
            let mut reader = WebsocketReader::new(read);
            let mut writer = WebsocketWriter::new(write);
            let rc = init_client(config, session_id, &mut reader, &mut writer).await;
            writer.shutdown().await?;
            if rc.is_err() {
                return rc;
            }
        }
        _ => {
            let _ = conn.shutdown(std::net::Shutdown::Both);
            error!("unknown schema:{}", conn_url.scheme());
            return Err(make_io_error("unknown url schema"));
        }
    }
    Ok(())
}

pub async fn get_rmux_stream(
    channel: &str,
    addr: String,
) -> Result<Box<dyn ChannelStream + Send>, std::io::Error> {
    let stream = create_stream(channel, "tcp", addr.as_str()).await?;
    Ok(Box::new(stream))
}
