use crate::config::TunnelConfig;
use crate::rmux::{
    handle_rmux_session, new_auth_event, read_rmux_event, AuthRequest, AuthResponse, CryptoContext,
};
use crate::utils::make_io_error;
use bytes::BytesMut;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

//use rand::Rng;
// use std::sync::atomic::{AtomicU32, Ordering};

pub async fn handle_rmux(
    tunnel_id: u32,
    mut inbound: TcpStream,
    cfg: TunnelConfig,
) -> Result<(), std::io::Error> {
    let key = String::from(cfg.cipher.as_ref().unwrap().key.as_str());
    let method = String::from(cfg.cipher.as_ref().unwrap().method.as_str());
    let mut rctx = CryptoContext::new(method.as_str(), key.as_str(), 0);
    let mut wctx = CryptoContext::new(method.as_str(), key.as_str(), 0);
    //1. auth connection
    let recv_ev = match read_rmux_event(&mut rctx, &mut inbound).await {
        Err(_) => return Err(make_io_error("can NOT read first auth envent.")),
        Ok(ev) => ev,
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
    inbound.write_all(&buf[..]).await?;
    let rctx = CryptoContext::new(auth_res.method.as_str(), key.as_str(), auth_res.rand);
    let wctx = CryptoContext::new(auth_res.method.as_str(), key.as_str(), auth_res.rand);
    handle_rmux_session("", tunnel_id, inbound, rctx, wctx, 0, cfg.relay_buf_size()).await?;
    Ok(())
}
