use byteorder::{ByteOrder, LittleEndian};
use bytes::{Buf, BufMut, Bytes, BytesMut, IntoBuf};
use tokio::codec::{Decoder, Encoder};

use std::io::{self, Cursor};

pub const FLAG_SYN: u8 = 0;
pub const FLAG_FIN: u8 = 1;
pub const FLAG_DATA: u8 = 2;
pub const FLAG_WIN_UPDATE: u8 = 3;
pub const FLAG_PING: u8 = 4;
pub const FLAG_PING_ACK: u8 = 5;

pub struct Header {
    pub version: u8,
    pub flags: u8,
    pub stream_id: u32,
    pub len: u32,
}

pub struct Event {
    pub header: Header,
    pub body: Vec<u8>,
}

pub fn new_fin_event(sid: u32) -> Event {
    Event {
        header: Header {
            version: 0,
            flags: FLAG_FIN,
            stream_id: sid,
            len: 0,
        },
        body: Vec::new(),
    }
}

pub fn new_data_event(sid: u32, buf: &[u8]) -> Event {
    Event {
        header: Header {
            version: 0,
            flags: FLAG_DATA,
            stream_id: sid,
            len: buf.len() as u32,
        },
        body: Vec::from(buf),
    }
}
pub fn new_window_update_event(sid: u32, len: u32) -> Event {
    Event {
        header: Header {
            version: 0,
            flags: FLAG_DATA,
            stream_id: sid,
            len: len,
        },
        body: Vec::new(),
    }
}

// This is where we'd keep track of any extra book-keeping information
// our transport needs to operate.
pub struct EventCodec;

// Turns string errors into std::io::Error
fn bad_utf8<E>(_: E) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "Unable to decode input as UTF8",
    )
}

impl EventCodec {
    pub fn new() -> Self {
        Self {}
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
        buf.reserve(ev.body.len() + 10);
        buf.put_u8(ev.header.version);
        buf.put_u8(ev.header.flags);
        buf.put_u32_le(ev.header.stream_id);
        let len = ev.body.len() as u32;
        buf.put_u32_le(len);
        if ev.body.len() > 0 {
            buf.put_slice(&ev.body[..]);
        }
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
    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Event>, io::Error> {
        if buf.len() < 10 {
            return Ok(None);
        }
        let mut src = Cursor::new(&mut *buf);
        //let mut nbuf = buf.into_buf();

        let ver = src.get_u8();
        let flags = src.get_u8();
        let sid = src.get_u32_le();
        let len = src.get_u32_le();
        if src.remaining() < len as usize {
            return Ok(None);
        }

        let mut ev = Event {
            header: Header {
                version: ver,
                flags: flags,
                stream_id: sid,
                len: len,
            },
            body: Vec::with_capacity(len as usize),
        };
        src.copy_to_slice(&mut ev.body[..]);
        unsafe {
            ev.body.set_len(len as usize);
        }
        Ok(Some(ev))
    }
}
