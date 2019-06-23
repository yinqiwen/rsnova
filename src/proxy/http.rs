use crate::common::io::*;
use crate::common::utils::*;
use crate::common::HttpProxyReader;
use crate::mux::relay::mux_relay_connection;
use crate::proxy::local::*;

use httparse::Status;
use tokio::prelude::*;
use tokio_io::io::write_all;

use tokio_io::{AsyncRead, AsyncWrite};

use url::Url;

fn handle_relay_connection<R, W>(
    ctx: LocalContext,
    reader: R,
    writer: W,
) -> impl Future<Item = (), Error = ()>
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    read_until_separator(reader, "\r\n\r\n")
        .map_err(|e| {
            error!("error:{}", e);
        })
        .and_then(move |(_reader, header_data, body_data)| {
            let mut headers = [httparse::EMPTY_HEADER; 32];
            let mut req = httparse::Request::new(&mut headers);
            let mut target: String = String::new();
            match req.parse(&header_data[..]) {
                Ok(Status::Complete(_)) => {
                    for h in req.headers {
                        if h.name.to_lowercase() == "host" {
                            target = String::from(String::from_utf8_lossy(h.value));
                            break;
                        }
                    }
                }
                _ => {
                    error!("parse http request error");
                    return future::Either::A(futures::future::err(()));
                }
            }
            if target.len() == 0 {
                if let Some(p) = req.path {
                    let mut url = String::from(p);
                    if !p.contains("://") {
                        if ctx.is_https {
                            url.insert_str(0, "https://");
                        } else {
                            url.insert_str(0, "http://");
                        }
                    }
                    if let Ok(u) = Url::parse(url.as_str()) {
                        if let Some(dst) = get_hostport_from_url(&u) {
                            target = dst;
                        }
                    }
                }
            }
            if target.len() == 0 {
                error!("No target address found.");
                return future::Either::A(futures::future::err(()));
            }
            if !target.contains(':') {
                if ctx.is_https {
                    target.push_str(":443");
                } else {
                    target.push_str(":80");
                }
            }
            //info!("Relay target:{}", target);

            if ctx.is_https {
                let conn_res = "HTTP/1.0 200 Connection established\r\n\r\n";
                let relay = write_all(writer, conn_res.as_bytes())
                    .map_err(|e| {
                        error!("write response error:{}", e);
                    })
                    .and_then(move |(_w, _)| {
                        mux_relay_connection(_reader, _w, "tcp", target.as_str(), 30, None, true)
                    });
                future::Either::B(future::Either::A(relay))
            } else {
                let mut all_data = header_data;
                all_data.extend_from_slice(&body_data[..]);
                let relay = mux_relay_connection(
                    _reader,
                    writer,
                    "tcp",
                    target.as_str(),
                    30,
                    Some(all_data),
                    true,
                );
                future::Either::B(future::Either::B(relay))
            }
        })
}

fn handle_normal_http_connection<R, W>(reader: R, writer: W) -> impl Future<Item = (), Error = ()>
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    read_until_separator(reader, "\r\n\r\n")
        .map_err(|e| {
            error!("error:{}", e);
        })
        .and_then(move |(_reader, header_data, body_data)| {
            let mut http_reader = HttpProxyReader::new(_reader);
            http_reader.add_recv_content(&header_data[..]);
            http_reader.add_recv_content(&body_data[..]);
            match http_reader.parse_request() {
                Ok(0) => {
                    error!("parse http request prtial");
                    return future::Either::A(futures::future::err(()));
                }
                Ok(_) => {
                    let target = String::from(http_reader.get_remote_addr());
                    return future::Either::B(mux_relay_connection(
                        http_reader,
                        writer,
                        "tcp",
                        target.as_str(),
                        30,
                        None,
                        true,
                    ));
                }
                Err(e) => {
                    error!("parse http request error:{}", e);
                    return future::Either::A(futures::future::err(()));
                }
            }
        })
}

pub fn handle_http_connection<R, W>(
    ctx: LocalContext,
    reader: R,
    writer: W,
) -> impl Future<Item = (), Error = ()>
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    if ctx.is_https || ctx.is_transparent() {
        future::Either::A(handle_relay_connection(ctx, reader, writer))
    //future::Either::A(futures::future::err(()))
    } else {
        future::Either::B(handle_normal_http_connection(reader, writer))
        //future::Either::B(futures::future::err(()))
    }
}
