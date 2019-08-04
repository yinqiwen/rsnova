use crate::mux::crypto::*;
use bytes::BytesMut;
use tokio::codec::{Decoder, Encoder};

use std::io::{Error, ErrorKind};

pub const FLAG_SYN: u8 = 1;
pub const FLAG_FIN: u8 = 2;
pub const FLAG_DATA: u8 = 3;
pub const FLAG_WIN_UPDATE: u8 = 4;
pub const FLAG_PING: u8 = 5;
pub const FLAG_AUTH: u8 = 6;
pub const FLAG_SHUTDOWN: u8 = 7;
pub const FLAG_PONG: u8 = 8;

pub const EVENT_HEADER_LEN: usize = 8;

#[derive(Debug, Clone)]
pub struct Header {
    pub flag_len: u32,
    pub stream_id: u32,
    //pub reserved: [u8; 2],
}

fn get_flag_len(len: u32, flag: u8) -> u32 {
    (len << 8) | u32::from(flag)
}

impl Header {
    fn set_flag_len(&mut self, len: u32, flag: u8) {
        self.flag_len = (len << 8) | u32::from(flag);
    }
    pub fn flags(&self) -> u8 {
        (self.flag_len & 0xFF) as u8
    }
    pub fn len(&self) -> u32 {
        (self.flag_len >> 8)
    }
    pub fn set_len(&mut self, v: u32) {
        let f = self.flags();
        self.set_flag_len(v, f);
    }
    pub fn set_flag(&mut self, v: u8) {
        let l = self.len();
        self.set_flag_len(l, v);
    }
}
#[derive(Debug, Clone)]
pub struct Event {
    pub header: Header,
    pub body: Vec<u8>,
}

impl Event {
    pub fn is_empty(&self) -> bool {
        self.header.flags() == 0 as u8
    }
}

pub fn new_empty_event() -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, 0),
            stream_id: 0,
        },
        body: Vec::new(),
    }
}

pub fn new_auth_event<T: serde::Serialize>(sid: u32, msg: &T) -> Event {
    let data = bincode::serialize(msg).unwrap();
    let mut ev = new_data_event(sid, &data[..]);
    ev.header.set_flag(FLAG_AUTH);
    ev
}

pub fn new_syn_event<T: serde::Serialize>(sid: u32, msg: &T) -> Event {
    let data = bincode::serialize(msg).unwrap();
    let mut ev = new_data_event(sid, &data[..]);
    ev.header.set_flag(FLAG_SYN);
    ev
}

pub fn new_fin_event(sid: u32) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_FIN),
            stream_id: sid,
        },
        body: Vec::new(),
    }
}

pub fn new_shutdown_event(sid: u32) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_SHUTDOWN),
            stream_id: sid,
        },
        body: Vec::new(),
    }
}

pub fn new_ping_event(sid: u32) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_PING),
            stream_id: sid,
        },
        body: Vec::new(),
    }
}
pub fn new_pong_event(sid: u32) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_PONG),
            stream_id: sid,
        },
        body: Vec::new(),
    }
}

pub fn new_data_event(sid: u32, buf: &[u8]) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(buf.len() as u32, FLAG_DATA),
            stream_id: sid,
        },
        body: Vec::from(buf),
    }
}
pub fn new_window_update_event(sid: u32, len: u32) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(len, FLAG_WIN_UPDATE),
            stream_id: sid,
        },
        body: Vec::new(),
    }
}

// This is where we'd keep track of any extra book-keeping information
// our transport needs to operate.
pub struct EventCodec {
    ctx: CryptoContext,
}

impl EventCodec {
    pub fn new(ctx: CryptoContext) -> Self {
        Self { ctx }
    }
}

// First, we implement encoding, because it's so straightforward.
// Just write out the bytes of the string followed by a newline!
// Easy-peasy.
impl Encoder for EventCodec {
    type Item = Event;
    type Error = std::io::Error;

    fn encode(&mut self, ev: Self::Item, buf: &mut BytesMut) -> Result<(), Self::Error> {
        // Note that we're given a BytesMut here to write into.
        // BytesMut comes from the bytes crate, and aims to give
        // efficient read/write access to a buffer. To use it,
        // we have to reserve memory before we try to write to it.
        //buf.reserve(line.len() + 1);
        // And now, we write out our stuff!
        // info!(
        //     "[{}]encode ev:{} with len {}:{}",
        //     ev.header.stream_id,
        //     ev.header.flags(),
        //     ev.header.len(),
        //     ev.body.len()
        // );
        self.ctx.encrypt(&ev, buf);
        Ok(())
    }
}

// The decoding is a little trickier, because we need to look for
// newline characters. We also need to handle *two* cases: the "normal"
// case where we're just asked to find the next string in a bunch of
// bytes, and the "end" case where the input has ended, and we need
// to find any remaining strings (the last of which may not end with a
// newline!
impl Decoder for EventCodec {
    type Item = Event;
    type Error = std::io::Error;

    // Find the next line in buf!
    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Event>, std::io::Error> {
        match self.ctx.decrypt(buf) {
            Ok(ev) => Ok(Some(ev)),
            Err((n, reason)) => {
                if reason.len() > 0 {
                    return Err(Error::from(ErrorKind::InvalidData));
                }
                Ok(None)
            }
        }
    }
}
