use crate::mux::event::*;
use crate::mux::message::ConnectRequest;

use crate::mux::session::*;
use tokio_io::{AsyncRead, AsyncWrite};

use std::collections::HashMap;
use tokio::codec::*;
use tokio::prelude::*;

use tokio::io;
use tokio::net::TcpStream;
use tokio_io::io::copy;

pub trait MuxStream {
    fn split<'a>(&'a self) -> (Box<dyn AsyncRead + 'a>, Box<dyn AsyncWrite + 'a>);
    fn close(&mut self);
    fn handle_recv_data(&mut self, data: Vec<u8>);
}

pub trait MuxSession {
    fn new_stream(&mut self, sid: u32) -> &MuxStream;
    fn get_stream(&mut self, sid: u32) -> Option<&mut MuxStream>;

    fn handle_fin(&mut self, ev: Event) {
        match self.get_stream(ev.header.stream_id) {
            Some(s) => {
                s.close();
            }
            None => {
                //
            }
        }
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
        let (r, w) = s.split();

        let connect_req: ConnectRequest = bincode::deserialize(&ev.body[..]).unwrap();
        let addr = connect_req.addr.parse().unwrap();
        let client = TcpStream::connect(&addr)
            .and_then(|stream| {
                let (reader, writer) = stream.split();
                let c1 = copy(r, writer);
                let c2 = copy(reader, w);
                let proxy = c1.join(c2).map_err(|err| {
                    //s.close();
                });
                // Process stream here.
                //tokio::run(proxy);
                Ok(())
            })
            .map_err(|err| {
                error!("connection error = {:?}", err);
                //s.close();
            });
    }
    fn handle_mux_event(&mut self, ev: Event) {
        match ev.header.flags {
            FLAG_SYN => {
                self.handle_syn(ev);
                //Ok(())
            }
            FLAG_FIN => {}
            FLAG_DATA => {
                self.handle_data(ev);
            }
            _ => {
                error!("invalid flags:{}", ev.header.flags);
                //Err("invalid flags")
                //Ok(())
            }
        }

    }
}

pub struct SessionManager {
    sessions: Vec<Box<dyn MuxSession>>,
    cursor: usize,
}

impl SessionManager {
    // pub fn new_mux_stream(&mut self) -> Option<&MuxStream> {
    //     if self.sessions.len() == 0 {
    //         return None;
    //     }
    //     self.cursor = self.cursor + 1;
    //     return Some(self.sessions[self.cursor % self.sessions.len()].open_stream());
    // }

    pub fn add_connection<T: AsyncRead + AsyncWrite + 'static>(&mut self, conn: T) {
        let session = CommonMuxSession::new(conn);

        let s: Box<dyn MuxSession> = Box::new(session);
        self.sessions.push(s);
    }
}
