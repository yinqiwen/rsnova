use crate::mux::event::*;
use crate::mux::message::*;
use crate::mux::stream::*;
use std::sync::{Arc, Mutex};

use std::time::Duration;
use tokio::net::TcpStream;

use tokio::io;
use tokio_io::io::{copy, read_exact, shutdown, write_all};
use tokio_io::AsyncRead;

use futures::Future;
use tokio_io_timeout::TimeoutReader;

pub type MuxStreamRef = Arc<Mutex<dyn MuxStream>>;

pub trait MuxSession: Send {
    fn new_stream(&mut self, sid: u32) -> MuxStreamRef;
    fn get_stream(&mut self, sid: u32) -> Option<MuxStreamRef>;
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
