use crate::config::*;
use crate::mux::channel::*;
use crate::mux::common::*;
use crate::mux::crypto::*;
use crate::mux::event::*;
use crate::mux::message::*;
use std::io::{Error, ErrorKind};

use std::time::Duration;
use tokio::net::TcpStream;

use tokio_io::io::write_all;
use tokio_io::io::{copy, shutdown};
use tokio_io::{AsyncRead, AsyncWrite};

use futures::Future;
use futures::Sink;
use futures::Stream;
use tokio_io_timeout::TimeoutReader;

use futures::sync::mpsc;

use bytes::BytesMut;

use tokio::codec::*;
use tokio::prelude::*;

use rand::Rng;

pub trait MuxStream: Sync + Send {
    fn split(&mut self) -> (Box<dyn AsyncRead + Send>, Box<dyn AsyncWrite + Send>);
    fn close(&mut self);
    fn handle_recv_data(&mut self, data: Vec<u8>);
    fn handle_window_update(&mut self, len: u32);
    fn write_event(&mut self, ev: Event);
    fn id(&self) -> u32;
}

//pub type MuxStreamRef = Box<dyn MuxStream>;

pub trait MuxSession: Send {
    fn new_stream(&mut self, sid: u32) -> &mut MuxStream;
    fn get_stream(&mut self, sid: u32) -> Option<&mut MuxStream>;
    fn next_stream_id(&mut self) -> u32;
    fn close_stream(&mut self, sid: u32, initial: bool);
    fn close(&mut self);

    fn open_stream(&mut self, proto: &str, addr: &str) -> &mut MuxStream {
        let next_id = self.next_stream_id();
        let s = self.new_stream(next_id);
        let c = ConnectRequest {
            proto: String::from(proto),
            addr: String::from(addr),
        };
        let cev = new_syn_event(next_id, &c, true);
        s.write_event(cev);
        s
    }

    fn handle_window_update(&mut self, ev: Event) {
        match self.get_stream(ev.header.stream_id) {
            Some(s) => {
                s.handle_window_update(ev.header.len());
            }
            None => {
                //
            }
        }
    }
    fn handle_fin(&mut self, ev: Event) {
        self.close_stream(ev.header.stream_id, false);
    }
    fn handle_data(&mut self, ev: Event) {
        match self.get_stream(ev.header.stream_id) {
            Some(s) => {
                s.handle_recv_data(ev.body);
            }
            None => {
                //
            }
        }
    }
    fn handle_syn(&mut self, ev: Event) {
        let s = self.new_stream(ev.header.stream_id);
        let (local_reader, local_writer) = s.split();
        let connect_req: ConnectRequest = bincode::deserialize(&ev.body[..]).unwrap();
        let addr = connect_req.addr.parse().unwrap();
        if connect_req.proto == "udp" {}
        let proxy = TcpStream::connect(&addr)
            .and_then(move |socket| {
                // let s = self.get_stream(sid).unwrap();
                let (remote_reader, remote_writer) = socket.split();
                let mut remote_reader = TimeoutReader::new(remote_reader);
                let mut local_reader = TimeoutReader::new(local_reader);
                let timeout = Duration::from_secs(get_config().lock().unwrap().read_timeout_sec);
                remote_reader.set_timeout(Some(timeout));
                local_reader.set_timeout(Some(timeout));
                let copy_to_remote = copy(local_reader, remote_writer)
                    .and_then(|(n, _, server_writer)| shutdown(server_writer).map(move |_| n));;
                let copy_to_local = copy(remote_reader, local_writer)
                    .and_then(|(n, _, client_writer)| shutdown(client_writer).map(move |_| n));;
                copy_to_remote.join(copy_to_local)
            })
            .map(move |(from_client, from_server)| {
                //self.close_stream(sid, true);
                info!(
                    "client at {} wrote {} bytes and received {} bytes",
                    addr, from_client, from_server
                );
            })
            .map_err(|e| {
                error!("error: {}", e);
                //local_writer.shutdown();
            });
        tokio::spawn(proxy);
    }
    fn handle_mux_event(&mut self, ev: Event) -> Option<Event> {
        match ev.header.flags() {
            FLAG_SYN => {
                self.handle_syn(ev);
                None
            }
            FLAG_FIN => {
                self.handle_fin(ev);
                None
            }
            FLAG_DATA => {
                self.handle_data(ev);
                None
            }
            FLAG_WIN_UPDATE => {
                self.handle_window_update(ev);
                None
            }
            _ => {
                error!("invalid flags:{}", ev.header.flags());
                None
            }
        }
    }
}

pub fn stream_event_rpc<T>(
    mut ctx: CryptoContext,
    conn: T,
    ev: Event,
) -> impl Future<Item = (CryptoContext, T, Event), Error = std::io::Error>
where
    T: AsyncRead + AsyncWrite,
{
    let mut buf = BytesMut::new();
    ctx.encrypt(&ev, &mut buf);
    let evbuf = buf.to_vec();
    write_all(conn, evbuf).and_then(|(conn, _)| read_encrypt_event(ctx, conn))
}

fn client_auth_session<T>(
    ctx: CryptoContext,
    conn: T,
    key: &str,
    method: &str,
) -> impl Future<Item = (CryptoContext, T, AuthResponse), Error = std::io::Error>
where
    T: AsyncRead + AsyncWrite,
{
    let sid = 0 as u32;
    let auth = AuthRequest {
        key: String::from(key),
        method: String::from(method),
    };
    stream_event_rpc(ctx, conn, new_auth_event(sid, &auth, true)).and_then(
        |(_ctx, conn, auth_res)| {
            let decoded: AuthResponse = bincode::deserialize(&auth_res.body[..]).unwrap();
            if !decoded.success {
                shutdown(conn);
                return Err(Error::from(ErrorKind::ConnectionRefused));
            }
            Ok((_ctx, conn, decoded))
        },
    )
}

fn server_auth_session<T>(
    ctx: CryptoContext,
    conn: T,
) -> impl Future<Item = (CryptoContext, T, AuthResponse), Error = std::io::Error>
where
    T: AsyncRead + AsyncWrite,
{
    read_encrypt_event(ctx, conn).and_then(|(mut _ctx, _conn, _ev)| {
        let mut rng = rand::thread_rng();
        let auth_res = AuthResponse {
            success: true,
            err: String::new(),
            rand: rng.gen::<u64>(),
            method: String::from(METHOD_CHACHA20_POLY1305),
        };
        let res = new_auth_event(0, &auth_res, true);
        let mut buf = BytesMut::new();
        _ctx.encrypt(&res, &mut buf);
        let evbuf = buf.to_vec();
        write_all(_conn, evbuf).map(move |(_conn, _)| (_ctx, _conn, auth_res))
    })
}

pub type SessionTaskClosure = Box<FnOnce(&mut dyn MuxSession) + Send>;

struct MuxConnectionProcessor<T: AsyncRead + AsyncWrite> {
    local_ev_send: mpsc::Sender<Event>,
    local_ev_recv: mpsc::Receiver<Event>,
    remote_ev_send: stream::SplitSink<Framed<T, EventCodec>>,
    remote_ev_recv: stream::SplitStream<Framed<T, EventCodec>>,
    task_send: mpsc::Sender<SessionTaskClosure>,
    task_recv: mpsc::Receiver<SessionTaskClosure>,
    session: ChannelMuxSession,
}

impl<T: AsyncRead + AsyncWrite> MuxConnectionProcessor<T> {
    fn new(ctx: CryptoContext, conn: T, is_client: bool) -> Self {
        let (atx, arx) = mpsc::channel(1024);
        let (send, recv) = mpsc::channel(1024);
        let session = ChannelMuxSession::new(&send, is_client);
        let (writer, reader) = Framed::new(conn, EventCodec::new(ctx)).split();
        Self {
            local_ev_send: send,
            local_ev_recv: recv,
            remote_ev_send: writer,
            remote_ev_recv: reader,
            task_send: atx,
            task_recv: arx,
            session: session,
        }
    }
    fn close(&mut self) {
        self.session.close();
        self.local_ev_send.close();
        self.remote_ev_send.close();
        self.task_send.close();
    }
}

impl<T: AsyncRead + AsyncWrite> Stream for MuxConnectionProcessor<T> {
    type Item = ();
    type Error = std::io::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let r1 = self.local_ev_recv.poll();
        match r1 {
            Ok(Async::Ready(v)) => match v {
                None => {
                    return Ok(Async::Ready(Some(())));
                }
                Some(ev) => {
                    if FLAG_FIN == ev.header.flags() {
                        self.session.close_stream(ev.header.stream_id, true);
                    }
                    self.remote_ev_send.start_send(ev);
                    return Ok(Async::Ready(Some(())));
                }
            },
            Err(_) => {
                error!("Local event recv error");
                self.close();
                return Err(Error::from(ErrorKind::ConnectionReset));
            }
            Ok(Async::NotReady) => {
                //
            }
        }
        let r2 = self.remote_ev_recv.poll();
        match r2 {
            Ok(Async::Ready(v)) => match v {
                None => {
                    return Ok(Async::Ready(Some(())));
                }
                Some(ev) => {
                    if let Some(res) = self.session.handle_mux_event(ev) {
                        self.remote_ev_send.start_send(res);
                    }
                    return Ok(Async::Ready(Some(())));
                }
            },
            Err(e) => {
                error!("Remote event recv error:{}", e);
                self.close();
                return Err(e);
            }
            Ok(Async::NotReady) => {
                //
            }
        }
        let r3 = self.task_recv.poll();
        match r3 {
            Ok(Async::Ready(v)) => match v {
                None => {
                    return Ok(Async::Ready(Some(())));
                }
                Some(func) => {
                    func(&mut self.session);
                    return Ok(Async::Ready(Some(())));
                }
            },
            Err(_) => {
                error!("Local task recv error");
                self.close();
                return Err(Error::from(ErrorKind::ConnectionReset));
            }
            Ok(Async::NotReady) => {
                //
            }
        }
        Ok(Async::NotReady)
    }
}

pub fn process_server_connection<T: AsyncRead + AsyncWrite>(
    conn: T,
) -> impl Future<Item = (), Error = ()> {
    let key = String::from(get_config().lock().unwrap().cipher.key.as_str());
    let method = String::from(get_config().lock().unwrap().cipher.method.as_str());
    let ctx = CryptoContext::new(method.as_str(), key.as_str(), 0);
    server_auth_session(ctx, conn)
        .map_err(|err| {
            error!("Faield to server auth connection with reason:{}", err);
        })
        .and_then(move |(_ctx, _conn, _res)| {
            let ctx = CryptoContext::new(_res.method.as_str(), key.as_str(), _res.rand);
            let processor = MuxConnectionProcessor::new(ctx, _conn, false);
            processor.for_each(|_| Ok(())).map_err(|_| {})
        })
}

pub fn process_client_connection<T: AsyncRead + AsyncWrite>(
    channel: &str,
    url: &str,
    conn: T,
) -> impl Future<Item = (), Error = ()> {
    let key = String::from(get_config().lock().unwrap().cipher.key.as_str());
    let method = String::from(get_config().lock().unwrap().cipher.method.as_str());
    let ctx = CryptoContext::new(METHOD_CHACHA20_POLY1305, key.as_str(), 0);
    let channel_str = String::from(channel);
    let url_str = String::from(url);
    client_auth_session(ctx, conn, key.as_str(), method.as_str())
        .map_err(|err| {
            error!("Faield to auth connection with reason:{}", err);
        })
        .and_then(move |(_, conn, res)| {
            info!("Connected server with rand:{}", res.rand);
            let ctx = CryptoContext::new(method.as_str(), key.as_str(), res.rand);
            let processor = MuxConnectionProcessor::new(ctx, conn, true);
            if add_session(channel_str.as_str(), url_str.as_str(), &processor.task_send) {
                future::Either::A(processor.for_each(|_| Ok(())).map_err(|_| {}))
            } else {
                future::Either::B(future::err(()))
            }
        })
}
