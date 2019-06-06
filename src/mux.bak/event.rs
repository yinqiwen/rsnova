use byteorder::{ByteOrder, LittleEndian};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio::codec::{Decoder, Encoder};

use std::io::{self, Cursor};
use tokio::prelude::*;

use tokio_io::io::{read_exact, write_all};
use tokio_io::{AsyncRead, AsyncWrite};
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

pub fn new_message_event<T: serde::Serialize>(sid: u32, msg: &T) -> Event {
    let data = bincode::serialize(msg).unwrap();
    new_data_event(sid, &data[..])
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

pub fn encode_event(ev: Event, buf: &mut BytesMut) {
    buf.reserve(ev.body.len() + 10);
    buf.put_u8(ev.header.version);
    buf.put_u8(ev.header.flags);
    buf.put_u32_le(ev.header.stream_id);
    let len = ev.body.len() as u32;
    buf.put_u32_le(len);
    if ev.body.len() > 0 {
        buf.put_slice(&ev.body[..]);
    }
}

pub fn read_event<T: AsyncRead>(r: T) -> impl Future<Item = Event, Error = std::io::Error> {
    let buf = vec![0; 10];
    read_exact(r, buf).and_then(|(_stream, data)| {
        let ver = data[0];
        let flags = data[1];
        let sid = byteorder::LittleEndian::read_u32(&data[2..6]);
        let elen = byteorder::LittleEndian::read_u32(&data[6..10]);
        let header = Header {
            version: ver,
            flags: flags,
            stream_id: sid,
            len: elen,
        };
        if FLAG_DATA != header.flags {
            let ev = Event {
                header: header,
                body: Vec::new(),
            };
            future::Either::A(future::ok(ev))
        } else {
            let data_buf = Vec::with_capacity(header.len as usize);
            let r = read_exact(_stream, data_buf).and_then(|(_r, _body)| {
                let ev = Event {
                    header: header,
                    body: _body,
                };
                Ok(ev)
            });
            future::Either::B(r)
        }
    })
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
        encode_event(ev, buf);
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
