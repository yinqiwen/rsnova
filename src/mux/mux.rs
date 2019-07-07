use super::relay::relay_connection;

use crate::config::*;
use crate::mux::channel::*;
use crate::mux::common::*;
use crate::mux::crypto::*;
use crate::mux::event::*;
use crate::mux::message::*;
use std::io::{Error, ErrorKind};

use tokio_io::io::shutdown;
use tokio_io::io::write_all;
use tokio_io::{AsyncRead, AsyncWrite};

use futures::Future;
use futures::Sink;
use futures::Stream;

use tokio::sync::mpsc;

use bytes::BytesMut;

use tokio::codec::*;
use tokio::prelude::*;

use rand::Rng;
use std::borrow::Cow;
use url::Url;

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
    fn new_stream(&mut self, sid: u32) -> &mut dyn MuxStream;
    fn get_stream(&mut self, sid: u32) -> Option<&mut dyn MuxStream>;
    fn next_stream_id(&mut self) -> u32;
    fn close_stream(&mut self, sid: u32, initial: bool);
    fn close(&mut self);
    fn ping(&mut self);
    fn num_of_streams(&self) -> usize;

    fn open_stream(&mut self, proto: &str, addr: &str) -> &mut dyn MuxStream {
        let next_id = self.next_stream_id();
        let s = self.new_stream(next_id);
        let c = ConnectRequest {
            proto: String::from(proto),
            addr: String::from(addr),
        };
        let cev = new_syn_event(next_id, &c);
        s.write_event(cev);
        s
    }

    fn handle_window_update(&mut self, ev: Event) {
        match self.get_stream(ev.header.stream_id) {
            Some(s) => {
                s.handle_window_update(ev.header.len());
            }
            None => {
                error!("No stream found for window event:{}", ev.header.stream_id);
            }
        }
    }
    fn handle_fin(&mut self, ev: Event) {
        self.close_stream(ev.header.stream_id, false);
    }
    fn handle_data(&mut self, ev: Event) {
        match self.get_stream(ev.header.stream_id) {
            Some(s) => {
                //info!("Recv data with len:{} {}", ev.header.len(), ev.body.len());
                s.handle_recv_data(ev.body);
            }
            None => {
                error!("No stream found for data event:{}", ev.header.stream_id);
            }
        }
    }
    fn handle_syn(&mut self, ev: Event) {
        let s = self.new_stream(ev.header.stream_id);
        let (local_reader, local_writer) = s.split();
        let connect_req: ConnectRequest = match bincode::deserialize(&ev.body[..]) {
            Ok(m) => m,
            Err(err) => {
                error!(
                    "Failed to parse ConnectRequest with error:{} while data len:{} {}",
                    err,
                    ev.body.len(),
                    ev.header.len(),
                );
                return;
            }
        };
        let relay = relay_connection(
            s.id(),
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
                //info!("Recv ping.");
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
        //key: String::from(key),
        method: String::from(method),
    };
    stream_event_rpc(ctx, conn, new_auth_event(sid, &auth)).and_then(|(_ctx, conn, auth_res)| {
        let decoded: AuthResponse = bincode::deserialize(&auth_res.body[..]).unwrap();
        if !decoded.success {
            shutdown(conn);
            return Err(Error::from(ErrorKind::ConnectionRefused));
        }
        Ok((_ctx, conn, decoded))
    })
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
        let auth_req: AuthRequest = match bincode::deserialize(&_ev.body[..]) {
            Ok(m) => m,
            Err(err) => {
                error!(
                    "Failed to parse ConnectRequest with error:{} while data len:{} {}",
                    err,
                    _ev.body.len(),
                    _ev.header.len(),
                );
                return future::Either::A(future::err(Error::from(ErrorKind::PermissionDenied)));
            }
        };
        let auth_res = AuthResponse {
            success: true,
            err: String::new(),
            rand: rng.gen::<u64>(),
            method: String::from(auth_req.method),
        };
        let res = new_auth_event(0, &auth_res);
        let mut buf = BytesMut::new();
        _ctx.encrypt(&res, &mut buf);
        let evbuf = buf.to_vec();
        future::Either::B(write_all(_conn, evbuf).map(move |(_conn, _)| (_ctx, _conn, auth_res)))
    })
}

pub type SessionTaskClosure = Box<dyn FnOnce(&mut dyn MuxSession) + Send>;

struct MuxConnectionProcessor<T: AsyncRead + AsyncWrite> {
    local_ev_send: mpsc::UnboundedSender<Event>,
    local_ev_recv: mpsc::UnboundedReceiver<Event>,
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
        let (send, recv) = mpsc::unbounded_channel();
        let session = ChannelMuxSession::new(&send, is_client);
        let (writer, reader) = Framed::new(conn, EventCodec::new(ctx)).split();
        Self {
            local_ev_send: send,
            local_ev_recv: recv,
            remote_ev_send: writer,
            remote_ev_recv: reader,
            task_send: atx,
            task_recv: arx,
            session,
            last_unsent_event: new_empty_event(),
            closed: false,
        }
    }
    fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.session.close();
            self.local_ev_recv.close();
            self.remote_ev_send.close();
            self.task_recv.close();
        }
    }

    fn try_send_event(&mut self, ev: Event) -> Result<bool, std::io::Error> {
        match self.remote_ev_send.start_send(ev) {
            Ok(AsyncSink::Ready) => Ok(true),
            Ok(AsyncSink::NotReady(v)) => {
                // info!(
                //     "##Not ready for ev {} {} {} {}",
                //     v.header.stream_id,
                //     v.header.flags(),
                //     v.header.len(),
                //     v.body.len()
                // );
                self.last_unsent_event = v;
                Ok(false)
            }
            Err(e) => {
                error!("Remote event send error:{}", e);
                self.close();
                Err(Error::from(ErrorKind::ConnectionReset))
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
                        // info!(
                        //     "[{}]recv local event:{} {}",
                        //     ev.header.stream_id,
                        //     ev.header.flags(),
                        //     ev.body.len(),
                        // );
                        if FLAG_SHUTDOWN == ev.header.flags() {
                            self.close();
                            return Err(Error::from(ErrorKind::ConnectionReset));
                        }
                        if FLAG_FIN == ev.header.flags() {
                            self.session.close_stream(ev.header.stream_id, true);
                        }
                        if let Err(e) = self.try_send_event(ev) {
                            error!("Remote event send error:{}", e);
                            self.close();
                            return Err(Error::from(ErrorKind::ConnectionReset));
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
                    // info!(
                    //     "[{}]recv remote event:{}, body len:{}",
                    //     ev.header.stream_id,
                    //     ev.header.flags(),
                    //     ev.body.len(),
                    // );
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
        match self.poll_local_event() {
            Err(e) => {
                return Err(e);
            }
            Ok(Async::Ready(_)) => {
                debug!("1 return with {}", all_not_ready);
                all_not_ready = false;
            }
            Ok(Async::NotReady) => {
                //
            }
        };
        match self.poll_remote_event() {
            Err(e) => {
                return Err(e);
            }
            Ok(Async::Ready(_)) => {
                debug!("0 return with {}", all_not_ready);
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
                debug!("2 return with {}", all_not_ready);
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
    let mut key = String::from(get_config().lock().unwrap().cipher.key.as_str());
    let mut method = String::from(get_config().lock().unwrap().cipher.method.as_str());
    if let Ok(u) = Url::parse(url) {
        for (k, v) in u.query_pairs() {
            match k {
                Cow::Borrowed("key") => {
                    key = String::from(v);
                }
                Cow::Borrowed("method") => {
                    method = String::from(v);
                }
                _ => {}
            }
        }
    }
    info!("Try to connect server with  method:{} key:{}", method, key);
    let ctx = CryptoContext::new(method.as_str(), key.as_str(), 0);
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
