use crate::mux::channel::*;
use crate::mux::event::*;
use crate::mux::message::*;
use std::sync::{Arc, Mutex};

use std::time::Duration;
use tokio::net::TcpStream;

use tokio_io::io::{copy, shutdown};
use tokio_io::io::{read_exact, write_all};
use tokio_io::{AsyncRead, AsyncWrite};

use futures::Future;
use futures::Stream;
use tokio_io_timeout::TimeoutReader;

use futures::sync::mpsc;

use bytes::BytesMut;

use tokio::codec::*;
use tokio::prelude::*;

pub trait MuxStream: Sync + Send {
    fn split(&mut self) -> (Box<dyn AsyncRead + Send>, Box<dyn AsyncWrite + Send>);
    fn close(&mut self);
    fn handle_recv_data(&mut self, data: Vec<u8>);
    fn handle_window_update(&mut self, len: u32);
    fn id(&self) -> u32;
}

//pub type MuxStreamRef = Box<dyn MuxStream>;

pub trait MuxSession: Send {
    fn new_stream(&mut self, sid: u32) -> &mut MuxStream;
    fn get_stream(&mut self, sid: u32) -> Option<&mut MuxStream>;
    fn open_stream(&mut self, proto: &str, addr: &str) -> &mut MuxStream;
    fn close_stream(&mut self, sid: u32, initial: bool);

    fn handle_window_update(&mut self, ev: Event) {
        match self.get_stream(ev.header.stream_id) {
            Some(s) => {
                s.handle_window_update(ev.header.len);
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
        let proxy = TcpStream::connect(&addr)
            .and_then(move |socket| {
                // let s = self.get_stream(sid).unwrap();
                let (remote_reader, remote_writer) = socket.split();
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
                //local_writer.shutdown();
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

pub fn stream_event_rpc<T>(
    conn: T,
    ev: Event,
) -> impl Future<Item = (T, Event), Error = std::io::Error>
where
    T: AsyncRead + AsyncWrite,
{
    let mut buf = BytesMut::new();
    encode_event(ev, &mut buf);
    let evbuf = buf.to_vec();
    write_all(conn, evbuf).and_then(|(conn, _)| read_event(conn))
}

fn auth_session<T>(
    conn: T,
    key: String,
) -> impl Future<Item = (T, AuthResponse), Error = std::io::Error>
where
    T: AsyncRead + AsyncWrite,
{
    let sid = 0 as u32;
    let auth = AuthRequest { key: key, rand: 0 };
    stream_event_rpc(conn, new_message_event(sid, &auth, true)).and_then(|(conn, auth_res)| {
        let decoded: AuthResponse = bincode::deserialize(&auth_res.body[..]).unwrap();
        Ok((conn, decoded))
    })
}

pub fn process_connection<T: AsyncRead + AsyncWrite>(conn: T) -> impl Future {
    let key = String::from("a");
    auth_session(conn, key)
        .map_err(|err| {})
        .and_then(|(conn, res)| {
            let (mut writer, reader) = Framed::new(conn, EventCodec::new()).split();
            let (send, recv) = mpsc::channel(1024);
            let mut session = ChannelMuxSession::new(&send);
            let rs = reader.map_err(|err| {
                error!("Error:{} occured.", err);
            });
            rs.select(recv).for_each(move |ev| {
                if ev.local {
                    if FLAG_FIN == ev.header.flags {
                        session.close_stream(ev.header.stream_id, true);
                    }
                    writer.start_send(ev);
                } else {
                    session.handle_mux_event(ev);
                }
                Ok(())
            })
        })
}
