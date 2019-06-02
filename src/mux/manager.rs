use crate::mux::event::*;
use crate::mux::message::ConnectRequest;

use crate::mux::session::*;
use tokio_io::{AsyncRead, AsyncWrite};

use tokio::prelude::*;

use tokio::io;
use tokio::net::TcpStream;
use tokio_udp::UdpSocket;

use std::time::Duration;
use tokio_io::io::{copy, shutdown};
use tokio_io_timeout::TimeoutReader;

use std::sync::{Arc, Mutex};

use url::{ParseError, Url};

use std::collections::HashMap;

pub trait MuxStream: Sync + Send {
    fn split(&self) -> (Box<dyn AsyncRead + Send>, Box<dyn AsyncWrite + Send>);
    fn close(&mut self);
    fn handle_recv_data(&mut self, data: Vec<u8>);
    fn handle_window_update(&mut self, len: u32);
}

pub trait MuxSession: Send {
    fn new_stream(&mut self, sid: u32) -> Arc<Mutex<dyn MuxStream>>;
    fn get_stream(&mut self, sid: u32) -> Option<Arc<Mutex<dyn MuxStream>>>;
    fn close_stream(&mut self, sid: u32, initial: bool);
    fn process_conn_events(&mut self) -> Result<(), io::Error>;

    fn handle_window_update(&mut self, ev: Event) {
        match self.get_stream(ev.header.stream_id) {
            Some(s) => {
                s.lock().unwrap().handle_window_update(ev.header.len);
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
                s.lock().unwrap().handle_recv_data(ev.body);
            }
            None => {
                //
            }
        }
    }
    fn handle_syn(&mut self, ev: Event) {
        let s = self.new_stream(ev.header.stream_id);
        let connect_req: ConnectRequest = bincode::deserialize(&ev.body[..]).unwrap();
        let addr = connect_req.addr.parse().unwrap();
        let proxy = TcpStream::connect(&addr)
            .and_then(move |socket| {
                // let s = self.get_stream(sid).unwrap();
                let (remote_reader, remote_writer) = socket.split();
                let (local_reader, local_writer) = s.lock().unwrap().split();
                let mut remote_reader = TimeoutReader::new(remote_reader);
                let mut local_reader = TimeoutReader::new(local_reader);
                let timeout = Duration::from_secs(10);
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
            });
        tokio::spawn(proxy);
    }
    fn handle_mux_event(&mut self, ev: Event) {
        match ev.header.flags {
            FLAG_SYN => {
                self.handle_syn(ev);
                //Ok(())
            }
            FLAG_FIN => {
                self.handle_fin(ev);
            }
            FLAG_DATA => {
                self.handle_data(ev);
            }
            FLAG_WIN_UPDATE => {
                self.handle_window_update(ev);
            }
            _ => {
                error!("invalid flags:{}", ev.header.flags);
                //Err("invalid flags")
                //Ok(())
            }
        }
    }
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

    fn add_connection<T: AsyncRead + AsyncWrite + Send + 'static>(&mut self, url: &Url, conn: T) {
        match self.remote_sessions.get_mut(url) {
            Some(holder) => {
                holder.add_connection(conn);
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

fn connect_tcp(r: SessionManagerRef, url: &Url) -> bool {
    //url.to_socket_addrs()
    let addr = "127.0.0.1:80".parse();
    let url_copy = Url::parse(url.as_str()).unwrap();
    let url_copy2 = Url::parse(url.as_str()).unwrap();
    let r2 = r.clone();
    match addr {
        Ok(remote) => {
            let work = TcpStream::connect(&remote)
                .and_then(move |socket| {
                    r.lock().unwrap().add_connection(&url_copy, socket);
                    Ok(())
                })
                .map_err(move |e| {
                    r2.lock().unwrap().connect_fail(&url_copy2);
                    error!("error: {}", e);
                });
            tokio::spawn(work);
            true
        }
        Err(e) => {
            //error!("{}", e)
            false
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
