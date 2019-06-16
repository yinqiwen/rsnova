use super::relay::relay_connection;
use crate::common::io::*;
use crate::common::udp::*;
use crate::common::utils::*;
use crate::config::*;
use crate::mux::channel::*;
use crate::mux::common::*;
use crate::mux::crypto::*;
use crate::mux::event::*;
use crate::mux::message::*;
use std::io::{Error, ErrorKind};

use std::time::Duration;
use tokio::net::TcpStream;
use tokio::net::UdpSocket;

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

fn proxy_stream<R>(
    local_reader: Box<dyn AsyncRead + Send>,
    local_writer: Box<dyn AsyncWrite + Send>,
    remote: R,
) -> impl Future<Item = (u64, u64), Error = std::io::Error>
where
    R: AsyncRead + AsyncWrite,
{
    let (remote_reader, remote_writer) = remote.split();
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
}

//pub type MuxStreamRef = Box<dyn MuxStream>;

pub trait MuxSession: Send {
    fn new_stream(&mut self, sid: u32) -> &mut dyn MuxStream;
    fn get_stream(&mut self, sid: u32) -> Option<&mut dyn MuxStream>;
    fn next_stream_id(&mut self) -> u32;
    fn close_stream(&mut self, sid: u32, initial: bool);
    fn close(&mut self);
    fn ping(&mut self);

    fn open_stream(&mut self, proto: &str, addr: &str) -> &mut dyn MuxStream {
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
        let connect_req: ConnectRequest = match bincode::deserialize(&ev.body[..]) {
            Ok(m) => m,
            Err(err) => {
                error!("Failed to parse ConnectRequest with error:{}", err);
                return;
            }
        };
        let relay = relay_connection(
            local_reader,
            local_writer,
            connect_req.proto.as_str(),
            connect_req.addr.as_str(),
            get_config().lock().unwrap().read_timeout_sec as u32,
            None,
        );

        tokio::spawn(relay);
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
            FLAG_PING => {
                //self.handle_window_update(ev);
                info!("Recv ping.");
                //self.ping();
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
    last_unsent_event: Event,
    closed: bool,
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
            last_unsent_event: new_empty_event(),
            closed: false,
        }
    }
    fn close(&mut self) {
        self.closed = true;
        self.session.close();
        self.local_ev_send.close();
        self.remote_ev_send.close();
        self.task_send.close();
    }

    fn try_send_event(&mut self, ev: Event) -> Result<bool, std::io::Error> {
        match self.remote_ev_send.start_send(ev) {
            Ok(AsyncSink::Ready) => {
                return Ok(true);
            }
            Ok(AsyncSink::NotReady(v)) => {
                self.last_unsent_event = v;
                Ok(false)
            }
            Err(e) => {
                error!("Remote event send error:{}", e);
                self.close();
                return Err(Error::from(ErrorKind::ConnectionReset));
            }
        }
    }

    fn poll_local_event(&mut self) -> Poll<Option<()>, std::io::Error> {
        let mut not_ready = true;
        if self.last_unsent_event.is_empty() {
            match self.local_ev_recv.poll() {
                Ok(Async::Ready(v)) => match v {
                    None => {
                        info!("local none event");
                        return Ok(Async::Ready(Some(())));
                    }
                    Some(ev) => {
                        not_ready = false;
                        debug!("recv local event:{}", ev.header.flags());
                        if FLAG_FIN == ev.header.flags() {
                            self.session.close_stream(ev.header.stream_id, true);
                        }
                        match self.try_send_event(ev) {
                            Err(e) => {
                                error!("Remote event send error:{}", e);
                                self.close();
                                return Err(Error::from(ErrorKind::ConnectionReset));
                            }
                            _ => {}
                        }
                        //return Ok(Async::Ready(Some(())));
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
            };
        } else {
            let sev = self.last_unsent_event.clone();
            match self.try_send_event(sev) {
                Err(e) => {
                    error!("Remote event send error:{}", e);
                    self.close();
                    return Err(Error::from(ErrorKind::ConnectionReset));
                }
                Ok(true) => {
                    self.last_unsent_event = new_empty_event();
                }
                _ => {}
            }
        }
        match self.local_ev_send.poll_complete() {
            Ok(Async::NotReady) => {
                //
            }
            Ok(Async::Ready(_)) => {
                //not_ready = false;
            }
            Err(_) => {
                error!("Local event flush error");
                self.close();
                return Err(Error::from(ErrorKind::ConnectionReset));
            }
        };
        if not_ready {
            Ok(Async::NotReady)
        } else {
            Ok(Async::Ready(Some(())))
        }
    }
    fn poll_remote_event(&mut self) -> Poll<Option<()>, std::io::Error> {
        let mut not_ready = true;
        match self.remote_ev_recv.poll() {
            Ok(Async::Ready(v)) => match v {
                None => {
                    warn!("recv none remote");
                    self.close();
                    return Err(Error::from(ErrorKind::ConnectionReset));
                }
                Some(ev) => {
                    info!("recv remote event:{}", ev.header.flags());
                    self.session.handle_mux_event(ev);
                    not_ready = false;
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
        };
        match self.remote_ev_send.poll_complete() {
            Ok(Async::NotReady) => {
                //
            }
            Ok(Async::Ready(_)) => {
                //not_ready = false;
            }
            Err(e) => {
                error!("Local event flush error:{}", e);
                self.close();
                return Err(Error::from(ErrorKind::ConnectionReset));
            }
        };
        if not_ready {
            Ok(Async::NotReady)
        } else {
            Ok(Async::Ready(Some(())))
        }
    }

    fn poll_task(&mut self) -> Poll<Option<()>, std::io::Error> {
        let mut not_ready = true;
        match self.task_recv.poll() {
            Ok(Async::Ready(v)) => match v {
                None => {
                    info!("recv none task");
                    return Ok(Async::Ready(Some(())));
                }
                Some(func) => {
                    debug!("recv task");
                    func(&mut self.session);
                    not_ready = false;
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
        };
        match self.task_send.poll_complete() {
            Ok(Async::NotReady) => {
                //
            }
            Ok(Async::Ready(_)) => {
                //not_ready = false;
            }
            Err(_) => {
                error!("Task event flush error");
                self.close();
                return Err(Error::from(ErrorKind::ConnectionReset));
            }
        };
        if not_ready {
            Ok(Async::NotReady)
        } else {
            Ok(Async::Ready(Some(())))
        }
    }
}

impl<T: AsyncRead + AsyncWrite> Stream for MuxConnectionProcessor<T> {
    type Item = ();
    type Error = std::io::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if self.closed {
            return Err(Error::from(ErrorKind::ConnectionReset));
        }
        let mut all_not_ready = true;
        match self.poll_remote_event() {
            Err(e) => {
                return Err(e);
            }
            Ok(Async::Ready(_)) => {
                info!("0 return with {}", all_not_ready);
                all_not_ready = false;
            }
            Ok(Async::NotReady) => {
                //
            }
        };
        match self.poll_local_event() {
            Err(e) => {
                return Err(e);
            }
            Ok(Async::Ready(_)) => {
                info!("1 return with {}", all_not_ready);
                all_not_ready = false;
            }
            Ok(Async::NotReady) => {
                //
            }
        };
        match self.poll_task() {
            Err(e) => {
                return Err(e);
            }
            Ok(Async::Ready(_)) => {
                info!("2 return with {}", all_not_ready);
                all_not_ready = false;
            }
            Ok(Async::NotReady) => {
                //
            }
        };
        if all_not_ready {
            Ok(Async::NotReady)
        } else {
            Ok(Async::Ready(Some(())))
        }
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
            let nctx = CryptoContext::new(_res.method.as_str(), key.as_str(), _res.rand);
            info!(
                "Recv connected client with method:{} rand:{}",
                _res.method, _res.rand
            );
            let processor = MuxConnectionProcessor::new(nctx, _conn, false);
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
            info!("Connected server with  method:{} rand:{}", method, res.rand);
            let ctx = CryptoContext::new(method.as_str(), key.as_str(), res.rand);
            let processor = MuxConnectionProcessor::new(ctx, conn, true);
            if add_session(channel_str.as_str(), url_str.as_str(), &processor.task_send) {
                future::Either::A(processor.for_each(|_| Ok(())).map_err(|_| {}))
            } else {
                future::Either::B(future::err(()))
            }
        })
}
