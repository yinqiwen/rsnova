use crate::config::TunnelConfig;
use crate::rmux::{
    new_auth_event, process_rmux_session, read_rmux_event, AuthRequest, AuthResponse,
    CryptoContext, MuxContext, DEFAULT_RECV_BUF_SIZE,
};
use crate::utils::{make_io_error, WebsocketReader, WebsocketWriter};
use bytes::BytesMut;
use futures::StreamExt;
use std::error::Error;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

pub async fn handle_websocket(
    tunnel_id: u32,
    mut inbound: TcpStream,
    cfg: TunnelConfig,
) -> Result<(), std::io::Error> {
    let mut buf = [0; 1024];
    let len = inbound.peek(&mut buf).await?;
    let req_str = match std::str::from_utf8(&buf[0..len]) {
        Err(e) => {
            return Err(make_io_error(&e.to_string()));
        }
        Ok(s) => s,
    };
    if let Some(first_line) = req_str.lines().next() {
        let mut headers = [httparse::EMPTY_HEADER; 4];
        let mut req = httparse::Request::new(&mut headers);
        let _ = req.parse(first_line.as_bytes());
        if let Some(path) = req.path {
            if path == "/" {
                let html = r#"
                <!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN"
                "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd">
            <html xmlns="http://www.w3.org/1999/xhtml" xml:lang="en" lang="en">
            <head>
                <meta http-equiv="Content-Type" content="text/html; charset=utf-8"/>
                <title>GSnova PAAS Server</title>
            </head>
            <body>
              <div id="container">
                <h1><a href="http://github.com/yinqiwen/rsnova">RSnova</a>
                  <span class="small">by <a href="http://twitter.com/yinqiwen">@yinqiwen</a></span></h1>
                <div class="description">
                  Welcome to use RSnova WebSocket Server!
                </div>
                <h2>Code</h2>
                <p>You can clone the project with <a href="http://git-scm.com">Git</a>
                  by running:
                  <pre>$ git clone https://github.com/yinqiwen/rsnova.git</pre>
                </p>
                <div class="footer">
                  get the source code on GitHub : <a href="http://github.com/yinqiwen/rsnova">yinqiwen/rsnova</a>
                </div>
              </div>
            </body>
            </html>
    "#;

                let res_content = format!(
                    "HTTP/1.0 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length:{}\r\n\r\n{}",
                    html.len(),
                    html
                );
                inbound.write_all(res_content.as_bytes()).await?;
                return Ok(());
            } else if path != "/relay" {
                let res_content = "HTTP/1.0 404 NotFound\r\n\r\n";
                inbound.write_all(res_content.as_bytes()).await?;
                return Ok(());
            }
        }
    }

    let ws_stream = match tokio_tungstenite::accept_async(inbound).await {
        Ok(s) => s,
        Err(e) => {
            return Err(make_io_error(&e.to_string()));
        }
    };
    let (write, read) = ws_stream.split();
    let reader = WebsocketReader::new(read);
    let mut writer = WebsocketWriter::new(write);
    let mut buf_reader = tokio::io::BufReader::with_capacity(DEFAULT_RECV_BUF_SIZE, reader);
    let key = String::from(cfg.cipher.as_ref().unwrap().key.as_str());
    let method = String::from(cfg.cipher.as_ref().unwrap().method.as_str());
    let mut rctx = CryptoContext::new(method.as_str(), key.as_str(), 0);
    let mut wctx = CryptoContext::new(method.as_str(), key.as_str(), 0);
    //1. auth connection
    let recv_ev = match read_rmux_event(&mut rctx, &mut buf_reader).await {
        Err(e) => return Err(make_io_error(&e.to_string())),
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
    writer.write_all(&buf[..]).await?;
    let rctx = CryptoContext::new(auth_res.method.as_str(), key.as_str(), auth_res.rand);
    let wctx = CryptoContext::new(auth_res.method.as_str(), key.as_str(), auth_res.rand);
    let ctx = MuxContext::new("", tunnel_id, rctx, wctx, 0);
    process_rmux_session(
        ctx,
        // "",
        // tunnel_id,
        &mut buf_reader,
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
