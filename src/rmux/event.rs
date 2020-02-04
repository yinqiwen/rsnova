//use tokio::codec::{Decoder, Encoder};

pub const FLAG_SYN: u8 = 1;
pub const FLAG_FIN: u8 = 2;
pub const FLAG_DATA: u8 = 3;
pub const FLAG_WIN_UPDATE: u8 = 4;
pub const FLAG_PING: u8 = 5;
pub const FLAG_AUTH: u8 = 6;
pub const FLAG_SHUTDOWN: u8 = 7;
pub const FLAG_PONG: u8 = 8;
pub const FLAG_ROUTINE: u8 = 9;

pub const EVENT_HEADER_LEN: usize = 8;

pub fn get_event_type_str(flags: u8) -> &'static str {
    match flags {
        FLAG_SYN => "FLAG_SYN",
        FLAG_FIN => "FLAG_FIN",
        FLAG_DATA => "FLAG_DATA",
        FLAG_WIN_UPDATE => "FLAG_WIN_UPDATE",
        FLAG_PING => "FLAG_PING",
        FLAG_AUTH => "FLAG_AUTH",
        FLAG_SHUTDOWN => "FLAG_SHUTDOWN",
        FLAG_PONG => "FLAG_PONG",
        _ => "INVALID",
    }
}

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
        (self.flag_len >> 8)
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
#[derive(Debug, Clone)]
pub struct Event {
    pub header: Header,
    pub body: Vec<u8>,
    pub remote: bool,
}

impl Event {
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.header.flags() == 0 as u8
    }
}

#[allow(dead_code)]
pub fn new_empty_event(remote: bool) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, 0),
            stream_id: 0,
        },
        body: Vec::new(),
        remote,
    }
}

pub fn new_auth_event<T: serde::Serialize>(sid: u32, msg: &T) -> Event {
    let data = bincode::serialize(msg).unwrap();
    let mut ev = new_data_event(sid, &data[..], false);
    ev.header.set_flag(FLAG_AUTH);
    ev
}

pub fn new_syn_event<T: serde::Serialize>(sid: u32, msg: &T) -> Event {
    let data = bincode::serialize(msg).unwrap();
    let mut ev = new_data_event(sid, &data[..], false);
    ev.header.set_flag(FLAG_SYN);
    ev
}

pub fn new_fin_event(sid: u32, remote: bool) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_FIN),
            stream_id: sid,
        },
        body: Vec::new(),
        remote,
    }
}

pub fn new_shutdown_event(sid: u32, remote: bool) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_SHUTDOWN),
            stream_id: sid,
        },
        body: Vec::new(),
        remote,
    }
}

pub fn new_routine_event(sid: u32) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_ROUTINE),
            stream_id: sid,
        },
        body: Vec::new(),
        remote: false,
    }
}

pub fn new_ping_event(sid: u32, remote: bool) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_PING),
            stream_id: sid,
        },
        body: Vec::new(),
        remote,
    }
}
pub fn new_pong_event(sid: u32, remote: bool) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_PONG),
            stream_id: sid,
        },
        body: Vec::new(),
        remote,
    }
}

pub fn new_data_event(sid: u32, buf: &[u8], remote: bool) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(buf.len() as u32, FLAG_DATA),
            stream_id: sid,
        },
        body: Vec::from(buf),
        remote,
    }
}
pub fn new_window_update_event(sid: u32, len: u32, remote: bool) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(len, FLAG_WIN_UPDATE),
            stream_id: sid,
        },
        body: Vec::new(),
        remote,
    }
}
