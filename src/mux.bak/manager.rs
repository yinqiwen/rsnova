use crate::mux::common::*;
use crate::mux::event::*;
use crate::mux::message::*;
use crate::mux::session::*;
use crate::mux::stream::*;

use bytes::BytesMut;

use tokio_io::io::{read_exact, write_all};
use tokio_io::{AsyncRead, AsyncWrite};

use tokio::prelude::*;

use tokio::net::TcpStream;
use tokio_udp::UdpSocket;

use std::sync::{Arc, Mutex};

use url::{ParseError, Url};

use std::collections::HashMap;

pub fn stream_event_rpc(
    stream: MuxStreamRef,
    ev: Event,
) -> impl Future<Item = Event, Error = std::io::Error> {
    let (reader, writer) = stream.lock().unwrap().split();
    let mut buf = BytesMut::new();
    encode_event(ev, &mut buf);
    let evbuf = buf.to_vec();
    write_all(writer, evbuf).and_then(|(w, _)| read_event(reader))
}

fn auth_connection<T: AsyncRead + AsyncWrite + Send + 'static>(
    conn: T,
    key: String,
) -> impl Future<Item = (AuthResponse, Box<dyn MuxSession + 'static>), Error = std::io::Error> {
    let mut session: Box<dyn MuxSession> = Box::new(CommonMuxSession::new(conn));
    let stream = session.new_stream(1);
    let sid = stream.lock().unwrap().id();
    let auth = AuthRequest { key: key, rand: 0 };
    stream_event_rpc(stream.clone(), new_message_event(sid, &auth)).and_then(move |auth_res| {
        let decoded: AuthResponse = bincode::deserialize(&auth_res.body[..]).unwrap();
        stream.lock().unwrap().close();
        Ok((decoded, session))
    })
}

fn connect_tcp(r: SessionManagerRef, url: &Url) -> bool {
    //url.to_socket_addrs()
    let remote_url = Url::parse(url.as_str()).unwrap();
    if let Some(addr) = ::common::utils::get_hostport_from_url(url) {}
    let remote = "127.0.0.1:80".parse().unwrap();
    //let session_url = Url::from_str(url.as_str());
    let work = TcpStream::connect(&remote)
        .and_then(move |socket| {
            let key = String::from("123");
            auth_connection(socket, key)
        })
        .and_then(move |(auth_res, session)| {
            r.lock().unwrap().add_session(&remote_url, session);

            Ok(())
        })
        .map_err(|err| {});
    tokio::spawn(work);
    return true;
}

struct MuxSessionHolder {
    connected: Vec<Box<dyn MuxSession>>,
    conns_per_host: u16,
    connectings: u16,
}

impl MuxSessionHolder {
    fn new(n: u16) -> Self {
        Self {
            connected: Vec::new(),
            conns_per_host: n,
            connectings: 0,
        }
    }

    fn add_connection<T: AsyncRead + AsyncWrite + Send + 'static>(&mut self, conn: T) {
        let session = CommonMuxSession::new(conn);
        let s: Box<dyn MuxSession> = Box::new(session);
        self.connected.push(s);
        self.connectings = self.connectings - 1;
    }
    fn connect_fail(&mut self) {
        self.connectings = self.connectings - 1;
    }
}

pub struct SessionManager {
    remote_sessions: HashMap<Url, MuxSessionHolder>,
    //sessions: Vec<Box<dyn MuxSession>>,
    cursor: usize,
}

pub type SessionManagerRef = Arc<Mutex<SessionManager>>;

impl SessionManager {
    pub fn new() -> SessionManagerRef {
        let m = SessionManager {
            remote_sessions: HashMap::new(),
            cursor: 0,
        };
        return Arc::new(Mutex::new(m));
    }
    // pub fn new_mux_stream(&mut self) -> Option<&mut MuxStream> {
    //     if self.sessions.len() == 0 {
    //         return None;
    //     }
    //     self.cursor = self.cursor + 1;
    //     return Some(self.sessions[self.cursor % self.sessions.len()].new_stream());
    // }

    fn add_session(&mut self, url: &Url, session: Box<dyn MuxSession>) {
        match self.remote_sessions.get_mut(url) {
            Some(holder) => {
                holder.connected.push(session);
            }
            None => {}
        }
    }

    fn connect_fail(&mut self, url: &Url) {
        match self.remote_sessions.get_mut(url) {
            Some(holder) => {
                holder.connect_fail();
            }
            None => {}
        }
    }
    pub fn add_url(&mut self, url: &str, conns_per_host: u16) -> Result<(), ParseError> {
        let u = Url::parse(url)?;
        let holder = MuxSessionHolder::new(conns_per_host);
        self.remote_sessions.insert(u, holder);
        Ok(())
    }

    pub fn process_conn_events(&mut self) {
        for (url, mut hodler) in self.remote_sessions.iter_mut() {
            hodler
                .connected
                .drain_filter(|s| match s.process_conn_events() {
                    Err(e) => {
                        error!("{}", e);
                        false
                    }
                    _ => true,
                });
        }
    }
}

fn connect_session(r: SessionManagerRef, url: &Url) {}

fn routine(r: SessionManagerRef) {
    let mut m = r.lock().unwrap();
    m.process_conn_events();
    for (url, mut holder) in m.remote_sessions.iter_mut() {
        while (holder.connected.len() as u16) + holder.connectings < holder.conns_per_host {
            //self.connect_tcp(url.)
            holder.connectings = holder.connectings + 1;
            connect_tcp(r.clone(), url);
        }
    }
}
