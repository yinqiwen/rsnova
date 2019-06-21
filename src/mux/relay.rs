use crate::common::future::FourEither;
use crate::common::udp::*;
use crate::common::utils::*;
use crate::common::MyTcpStream;

use super::channel::select_session;
use super::mux::MuxSession;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio_io::io::{copy, shutdown};
use tokio_io::{AsyncRead, AsyncWrite};

use std::time::Duration;
use tokio::net::TcpStream;
use tokio::net::UdpSocket;

use bytes::Bytes;

use std::net::{SocketAddr, ToSocketAddrs};

use tokio_io::io::write_all;

use tokio::prelude::*;
use tokio_io_timeout::TimeoutReader;

fn proxy_stream<R, W, A, B>(
    local_reader: A,
    local_writer: B,
    remote_reader: R,
    remote_writer: W,
    timeout_secs: u32,
    initial_data: Option<Bytes>,
    close_on_local_eof: bool,
) -> impl Future<Item = (u64, u64), Error = std::io::Error>
where
    R: AsyncRead,
    W: AsyncWrite,
    A: AsyncRead,
    B: AsyncWrite,
{
    //let (remote_reader, remote_writer) = remote.split();
    let mut remote_reader = TimeoutReader::new(remote_reader);
    let mut local_reader = TimeoutReader::new(local_reader);
    let timeout = Duration::from_secs(timeout_secs as u64);
    remote_reader.set_timeout(Some(timeout));
    local_reader.set_timeout(Some(timeout));

    let close_local = Arc::new(AtomicBool::new(true));

    let preprocess = match initial_data {
        Some(data) => future::Either::A(
            write_all(remote_writer, data).and_then(|(_remote_writer, _)| Ok(_remote_writer)),
        ),
        None => future::Either::B(future::ok::<_, std::io::Error>(remote_writer)),
    };
    let close_local2 = close_local.clone();
    let should_close_on_local_eof = Arc::new(close_on_local_eof);
    preprocess.and_then(|_remote_writer| {
        let copy_to_remote =
            copy(local_reader, _remote_writer).and_then(move |(n, _, server_writer)| {
                //
                info!("###local read done!");
                if !should_close_on_local_eof.as_ref() {
                    close_local2.store(false, Ordering::SeqCst);
                }
                shutdown(server_writer).map(move |_| {
                    //debug!("###local shutdown done!");
                    n
                })
            });
        let copy_to_local =
            copy(remote_reader, local_writer).and_then(move |(n, _, client_writer)| {
                //
                info!("####remote read done!");
                if close_local.load(Ordering::SeqCst) {
                    future::Either::A(future::ok::<u64, std::io::Error>(n))
                } else {
                    future::Either::B(shutdown(client_writer).map(move |_| {
                        //debug!("###local shutdown done!");
                        n
                    }))
                }
            });
        copy_to_local.join(copy_to_remote)
    })
}

pub fn relay_connection<R, W>(
    local_reader: R,
    local_writer: W,
    proto: &str,
    origin_addr: &str,
    timeout_secs: u32,
    initial_data: Option<Bytes>,
    close_on_local_eof: bool,
) -> impl Future<Item = (), Error = ()>
where
    R: AsyncRead,
    W: AsyncWrite,
{
    let raddr: Vec<SocketAddr> = match origin_addr.to_socket_addrs() {
        Ok(m) => m.collect(),
        Err(err) => {
            error!(
                "Failed to parse addr with error:{} from connect request:{}",
                err, origin_addr
            );
            return FourEither::A(futures::future::err(()));
        }
    };
    let oaddr = String::from(origin_addr);
    let addr: SocketAddr = raddr[0];
    // let addr: SocketAddr = addr.parse().unwrap();
    debug!("{:?}", addr);

    if proto == "udp" {
        let lport = get_available_udp_port();
        let laddr = format!("0.0.0.0:{}", lport).parse().unwrap();
        let u = match UdpSocket::bind(&laddr) {
            Ok(m) => m,
            Err(err) => {
                error!("Failed to bind udp addr:{} with error:{}", laddr, err);
                return FourEither::A(futures::future::err(()));
            }
        };
        if let Err(e) = u.connect(&addr) {
            error!("Failed to connect udp addr:{} with error:{}", addr, e);
            return FourEither::D(futures::future::err(()));
        }
        let conn = UdpConnection::new(u);
        let (conn_r, conn_w) = conn.split();
        FourEither::B(
            proxy_stream(
                local_reader,
                local_writer,
                conn_r,
                conn_w,
                timeout_secs,
                initial_data,
                close_on_local_eof,
            )
            .map_err(|e| {
                error!("udp proxy error: {}", e);
            })
            .map(move |(from_client, from_server)| {
                info!(
                    "client at {} wrote {} bytes and received {} bytes",
                    addr, from_client, from_server
                );
            }),
        )
    } else {
        debug!("Connect tcp:{}", addr);
        FourEither::C(
            TcpStream::connect(&addr)
                .and_then(move |socket| {
                    debug!(
                        "Connected tcp socket {}  {}",
                        socket.local_addr().unwrap(),
                        socket.peer_addr().unwrap()
                    );
                    let (conn_r, conn_w) = MyTcpStream::new(socket).split();
                    proxy_stream(
                        local_reader,
                        local_writer,
                        conn_r,
                        conn_w,
                        timeout_secs,
                        initial_data,
                        close_on_local_eof,
                    )
                })
                .map(move |(from_client, from_server)| {
                    //self.close_stream(sid, true);
                    info!(
                        "client to {} wrote {} bytes and received {} bytes",
                        oaddr, from_client, from_server
                    );
                })
                .map_err(|e| {
                    error!("tcp proxy error: {}", e);
                    //local_writer.shutdown();
                }),
        )
    }
}

pub fn mux_relay_connection<R, W>(
    local_reader: R,
    local_writer: W,
    proto: &str,
    addr: &str,
    timeout_secs: u32,
    initial_data: Option<Bytes>,
    close_on_local_eof: bool,
) -> impl Future<Item = (), Error = ()>
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    if let Some(mut session_task) = select_session() {
        let proto_str = String::from(proto);
        let addr_str = String::from(addr);
        let t = move |session: &mut dyn MuxSession| {
            let mut remote = session.open_stream(proto_str.as_str(), addr_str.as_str());
            let (remote_r, remote_w) = remote.split();
            let relay = proxy_stream(
                local_reader,
                local_writer,
                remote_r,
                remote_w,
                timeout_secs,
                initial_data,
                close_on_local_eof,
            )
            .map(|_| {
                //
            })
            .map_err(|e| {
                //
            });
            tokio::spawn(relay);
        };
        session_task.start_send(Box::new(t));
        session_task.poll_complete();
        future::Either::A(future::ok::<(), ()>(()))
    } else {
        future::Either::B(relay_connection(
            local_reader,
            local_writer,
            proto,
            addr,
            timeout_secs,
            initial_data,
            close_on_local_eof,
        ))
    }
}
