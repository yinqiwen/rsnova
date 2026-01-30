// use std::io::IoSlice;

//use tokio::codec::{Decoder, Encoder};
use anyhow::Result;
use bincode::{config, Decode, Encode};
// use bytes::{BufMut, BytesMut};
// use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const FLAG_OPEN: u8 = 1;
pub const FLAG_FIN: u8 = 2;
pub const FLAG_SYN: u8 = 4;
pub const FLAG_DATA: u8 = 3;
// pub const FLAG_WIN_UPDATE: u8 = 4;
pub const FLAG_PING: u8 = 5;
pub const FLAG_SHUTDOWN: u8 = 7;
// pub const FLAG_PONG: u8 = 8;
// pub const FLAG_ROUTINE: u8 = 9;

pub const EVENT_HEADER_LEN: usize = 8;
pub const MAX_EVENT_BODY_LEN: u32 = 16 * 1024 * 1024; // 16MB

// pub fn get_event_type_str(flags: u8) -> &'static str {
//     match flags {
//         FLAG_OPEN => "FLAG_SYN",
//         // FLAG_FIN => "FLAG_FIN",
//         // FLAG_DATA => "FLAG_DATA",
//         // FLAG_WIN_UPDATE => "FLAG_WIN_UPDATE",
//         // FLAG_PING => "FLAG_PING",
//         // FLAG_SHUTDOWN => "FLAG_SHUTDOWN",
//         // FLAG_PONG => "FLAG_PONG",
//         _ => "INVALID",
//     }
// }

#[derive(Debug, Clone, Copy)]
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
        self.flag_len >> 8
    }
    #[allow(dead_code)]
    pub fn set_len(&mut self, v: u32) {
        let f = self.flags();
        self.set_flag_len(v, f);
    }
    pub fn set_flag(&mut self, v: u8) {
        let l = self.len();
        self.set_flag_len(l, v);
    }
}

#[derive(Encode, Decode, PartialEq, Debug)]
pub struct OpenStreamEvent {
    pub proto: String,
    pub addr: String,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub header: Header,
    pub body: Vec<u8>,
}

impl Event {
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.header.flags() == 0_u8
    }
}

#[allow(dead_code)]
pub fn new_empty_event() -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, 0),
            stream_id: 0,
        },
        body: Vec::new(),
    }
}
fn new_event(sid: u32, buf: &[u8]) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(buf.len() as u32, 0),
            stream_id: sid,
        },
        body: Vec::from(buf),
    }
}
pub fn new_data_event(sid: u32, buf: Vec<u8>) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(buf.len() as u32, FLAG_DATA),
            stream_id: sid,
        },
        body: buf,
    }
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
pub fn new_syn_event(sid: u32) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_SYN),
            stream_id: sid,
        },
        body: Vec::new(),
    }
}
pub fn new_ping_event() -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_PING),
            stream_id: 0,
        },
        body: Vec::new(),
    }
}

pub fn new_open_stream_event(sid: u32, msg: &OpenStreamEvent) -> Event {
    let config = config::standard();
    let data: Vec<u8> = bincode::encode_to_vec(msg, config).unwrap();
    let mut ev = new_event(sid, &data[..]);
    ev.header.set_flag(FLAG_OPEN);
    ev
}

pub async fn write_event<T>(writer: &mut T, ev: Event) -> anyhow::Result<()>
where
    T: AsyncWriteExt + Unpin,
{
    let mut hbuf = [0u8; 8];
    hbuf[0..4].copy_from_slice(&ev.header.flag_len.to_le_bytes());
    hbuf[4..8].copy_from_slice(&ev.header.stream_id.to_le_bytes());
    writer.write_all(&hbuf).await?;
    if !ev.body.is_empty() {
        writer.write_all(&ev.body).await?;
    }
    // if !ev.body.is_empty() {
    //     let all = [IoSlice::new(&hbuf), IoSlice::new(&ev.body)];
    //     writer.write_vectored(&all).await?;
    // } else {
    //     writer.write_all(&hbuf).await?;
    // }

    // let all = if !ev.body.is_empty() {
    //     vec![IoSlice::new(&hbuf), IoSlice::new(&ev.body)]
    // } else {
    //     vec![IoSlice::new(&hbuf)]
    // };
    // writer.write_vectored(&all).await?;
    // Ok(())
    // let mut out = BytesMut::new();
    // out.reserve(EVENT_HEADER_LEN + ev.body.len());
    // out.put_u32_le(ev.header.flag_len);
    // out.put_u32_le(ev.header.stream_id);
    // if !ev.body.is_empty() {
    //     out.put_slice(&ev.body[..]);
    // }
    // writer.write_all(&out).await?;
    Ok(())
}

pub async fn read_event<T>(reader: &mut T) -> Result<Event, std::io::Error>
where
    T: AsyncReadExt + Unpin + ?Sized,
{
    let mut hbuf = [0; EVENT_HEADER_LEN];
    reader.read_exact(&mut hbuf).await?;

    let header = Header {
        flag_len: u32::from_le_bytes(hbuf[0..4].try_into().unwrap()),
        stream_id: u32::from_le_bytes(hbuf[4..8].try_into().unwrap()),
    };
    let body_data_len = header.len();
    if body_data_len > MAX_EVENT_BODY_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("event body too large: {}", body_data_len),
        ));
    }
    let mut dbuf = vec![0; body_data_len as usize];
    if body_data_len > 0 {
        let _ = reader.read_exact(&mut dbuf).await?;
    }
    let ev = Event { header, body: dbuf };
    Ok(ev)
}
