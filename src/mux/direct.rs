use crate::mux::event::*;
use crate::mux::mux::*;
use futures::sync::mpsc;
use tokio::net::TcpStream;
use tokio_io::{AsyncRead, AsyncWrite};

struct DirectMuxSession;
struct DirectStream {
    conn: TcpStream,
}

impl MuxStream for DirectStream {
    fn split(&mut self) -> (Box<dyn AsyncRead + Send>, Box<dyn AsyncWrite + Send>) {
        self.conn.split()
    }
    fn close(&mut self) {}
    fn handle_recv_data(&mut self, data: Vec<u8>) {
        //do nothing
    }
    fn handle_window_update(&mut self, len: u32) {
        //do nothing
    }
    fn write_event(&mut self, ev: Event) {
        //do nothing
    }
    fn id(&self) -> u32 {
        0
    }
}

impl MuxSession for DirectMuxSession {
    fn open_stream(&mut self, proto: &str, addr: &str) -> &mut dyn MuxStream {}
    fn new_stream(&mut self, sid: u32) -> &mut dyn MuxStream {}
    fn get_stream(&mut self, sid: u32) -> Option<&mut dyn MuxStream> {
        None
    }
    fn next_stream_id(&mut self) -> u32 {
        0
    }
    fn close_stream(&mut self, sid: u32, initial: bool) {
        //
    }
    fn close(&mut self) {
        //
    }
    fn ping(&mut self) {
        //
    }
}

pub fn init_direct_mux_session() {}
