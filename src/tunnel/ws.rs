use crate::config::TunnelConfig;
use crate::rmux::{
    new_auth_event, process_rmux_session, read_encrypt_event, AuthRequest, AuthResponse,
    CryptoContext, MuxContext,
};
use crate::utils::{make_io_error, WebsocketReader, WebsocketWriter};
use bytes::BytesMut;
use futures::StreamExt;
use std::error::Error;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

pub async fn handle_websocket(
    tunnel_id: u32,
    inbound: TcpStream,
    cfg: TunnelConfig,
) -> Result<(), std::io::Error> {
    let ws_stream = match tokio_tungstenite::accept_async(inbound).await {
        Ok(s) => s,
        Err(e) => {
            return Err(make_io_error(e.description()));
        }
    };
    let (write, read) = ws_stream.split();
    let mut reader = WebsocketReader::new(read);
    let mut writer = WebsocketWriter::new(write);
    let key = String::from(cfg.cipher.as_ref().unwrap().key.as_str());
    let method = String::from(cfg.cipher.as_ref().unwrap().method.as_str());
    let mut rctx = CryptoContext::new(method.as_str(), key.as_str(), 0);
    let mut wctx = CryptoContext::new(method.as_str(), key.as_str(), 0);
    //1. auth connection
    let mut recv_buf = BytesMut::new();
    let recv_ev = match read_encrypt_event(&mut rctx, &mut reader, &mut recv_buf).await {
        Err(e) => return Err(make_io_error(e.description())),
        Ok(Some(ev)) => ev,
        Ok(None) => {
            return Err(make_io_error("can NOT read first auth envent."));
        }
    };
    let auth_req: AuthRequest = match bincode::deserialize(&recv_ev.body[..]) {
        Ok(m) => m,
        Err(err) => {
            error!(
                "Failed to parse AuthRequest with error:{} while data len:{} {}",
                err,
                recv_ev.body.len(),
                recv_ev.header.len(),
            );
            return Err(make_io_error("Failed to parse AuthRequest"));
        }
    };
    //let mut rng = rand::thread_rng();
    let auth_res = AuthResponse {
        success: true,
        err: String::new(),
        rand: rand::random::<u64>(),
        //rand: 1,
        method: auth_req.method,
    };
    let mut res = new_auth_event(0, &auth_res);
    let mut buf = BytesMut::new();
    wctx.encrypt(&mut res, &mut buf);
    writer.write_all(&buf[..]).await?;
    let rctx = CryptoContext::new(auth_res.method.as_str(), key.as_str(), auth_res.rand);
    let wctx = CryptoContext::new(auth_res.method.as_str(), key.as_str(), auth_res.rand);
    let ctx = MuxContext::new("", tunnel_id, rctx, wctx, 0, &mut recv_buf);
    process_rmux_session(
        ctx,
        // "",
        // tunnel_id,
        &mut reader,
        &mut writer,
        // rctx,
        // wctx,
        // &mut recv_buf,
        // 0,
        cfg.relay_buf_size(),
    )
    .await?;
    Ok(())
}
