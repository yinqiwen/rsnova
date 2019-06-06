use crate::mux::event::*;
use crate::mux::message::*;

use bytes::{Bytes, BytesMut, IntoBuf};
use tokio_io::io::{read_exact, write_all};
use tokio_io::{AsyncRead, AsyncWrite};

use tokio::prelude::*;

pub trait MuxStream: Sync + Send {
    fn split(&self) -> (Box<dyn AsyncRead + Send>, Box<dyn AsyncWrite + Send>);
    fn close(&mut self);
    fn handle_recv_data(&mut self, data: Vec<u8>);
    fn handle_window_update(&mut self, len: u32);
    fn id(&self) -> u32;
}
