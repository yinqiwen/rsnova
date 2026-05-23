use anyhow::Result;
use bincode::{config, Decode, Encode};
use bytes::{Bytes, BytesMut};
use std::io::IoSlice;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const FLAG_OPEN: u8 = 1;
pub const FLAG_FIN: u8 = 2;
pub const FLAG_SYN: u8 = 4;
pub const FLAG_DATA: u8 = 3;
pub const FLAG_PING: u8 = 5;
pub const FLAG_SHUTDOWN: u8 = 7;

pub const FLAG_AUTH: u8 = 6;
pub const FLAG_AUTH_ACK: u8 = 9;
pub const FLAG_REVERSE_OPEN: u8 = 10;

pub const EVENT_HEADER_LEN: usize = 8;
pub const MAX_EVENT_BODY_LEN: u32 = 256 * 1024; // 256KB (was 16MB, reduced for embedded)

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

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub enum AuthRequest {
    Proxy,
    Register(RegisterRequest),
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub struct RegisterRequest {
    pub client_id: String,
    pub tunnels: Vec<TunnelEntry>,
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub struct TunnelEntry {
    pub local_addr: String,
    pub remote_port: u16,
    pub sni: Option<String>,
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub enum AuthAck {
    Proxy,
    RegisterAck(RegisterAck),
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub struct RegisterAck {
    pub results: Vec<TunnelResult>,
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub struct TunnelResult {
    pub success: bool,
    pub remote_port: u16,
    pub sni: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub header: Header,
    pub body: Bytes,
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
        body: Bytes::new(),
    }
}
fn new_event(sid: u32, buf: Bytes) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(buf.len() as u32, 0),
            stream_id: sid,
        },
        body: buf,
    }
}
pub fn new_data_event(sid: u32, buf: Bytes) -> Event {
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
        body: Bytes::new(),
    }
}
pub fn new_shutdown_event(sid: u32) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_SHUTDOWN),
            stream_id: sid,
        },
        body: Bytes::new(),
    }
}
pub fn new_syn_event(sid: u32) -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_SYN),
            stream_id: sid,
        },
        body: Bytes::new(),
    }
}
pub fn new_ping_event() -> Event {
    Event {
        header: Header {
            flag_len: get_flag_len(0, FLAG_PING),
            stream_id: 0,
        },
        body: Bytes::new(),
    }
}

pub fn new_open_stream_event(sid: u32, msg: &OpenStreamEvent) -> anyhow::Result<Event> {
    let config = config::standard();
    let data: Vec<u8> = bincode::encode_to_vec(msg, config)
        .map_err(|e| anyhow::anyhow!("encode open stream event failed: {}", e))?;
    let mut ev = new_event(sid, Bytes::from(data));
    ev.header.set_flag(FLAG_OPEN);
    Ok(ev)
}

pub fn new_auth_event(sid: u32, req: &AuthRequest) -> anyhow::Result<Event> {
    let config = config::standard();
    let data: Vec<u8> = bincode::encode_to_vec(req, config)
        .map_err(|e| anyhow::anyhow!("encode auth request failed: {}", e))?;
    let mut ev = new_event(sid, Bytes::from(data));
    ev.header.set_flag(FLAG_AUTH);
    Ok(ev)
}

pub fn new_auth_ack_event(sid: u32, ack: &AuthAck) -> anyhow::Result<Event> {
    let config = config::standard();
    let data: Vec<u8> = bincode::encode_to_vec(ack, config)
        .map_err(|e| anyhow::anyhow!("encode auth ack failed: {}", e))?;
    let mut ev = new_event(sid, Bytes::from(data));
    ev.header.set_flag(FLAG_AUTH_ACK);
    Ok(ev)
}

pub fn new_reverse_open_stream_event(sid: u32, msg: &OpenStreamEvent) -> anyhow::Result<Event> {
    let config = config::standard();
    let data: Vec<u8> = bincode::encode_to_vec(msg, config)
        .map_err(|e| anyhow::anyhow!("encode reverse open stream event failed: {}", e))?;
    let mut ev = new_event(sid, Bytes::from(data));
    ev.header.set_flag(FLAG_REVERSE_OPEN);
    Ok(ev)
}

async fn write_all_vectored<T>(
    writer: &mut T,
    mut header: &[u8],
    mut body: &[u8],
) -> std::io::Result<()>
where
    T: AsyncWrite + Unpin,
{
    while !header.is_empty() || !body.is_empty() {
        let bufs = [IoSlice::new(header), IoSlice::new(body)];
        let n = writer.write_vectored(&bufs).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write event",
            ));
        }
        if n >= header.len() {
            let body_n = n - header.len();
            header = &[];
            body = &body[body_n..];
        } else {
            header = &header[n..];
        }
    }
    Ok(())
}

pub async fn write_event<T>(writer: &mut T, ev: Event) -> anyhow::Result<()>
where
    T: AsyncWrite + Unpin,
{
    let mut hbuf = [0u8; 8];
    hbuf[0..4].copy_from_slice(&ev.header.flag_len.to_le_bytes());
    hbuf[4..8].copy_from_slice(&ev.header.stream_id.to_le_bytes());
    if ev.body.is_empty() {
        writer.write_all(&hbuf).await?;
    } else {
        write_all_vectored(writer, &hbuf, ev.body.as_ref()).await?;
    }
    Ok(())
}

pub async fn read_event<T>(reader: &mut T) -> Result<Event, std::io::Error>
where
    T: AsyncReadExt + Unpin + ?Sized,
{
    let mut hbuf = [0; EVENT_HEADER_LEN];
    reader.read_exact(&mut hbuf).await?;

    let header = Header {
        flag_len: u32::from_le_bytes([hbuf[0], hbuf[1], hbuf[2], hbuf[3]]),
        stream_id: u32::from_le_bytes([hbuf[4], hbuf[5], hbuf[6], hbuf[7]]),
    };
    let body_data_len = header.len();
    if body_data_len > MAX_EVENT_BODY_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("event body too large: {}", body_data_len),
        ));
    }
    let mut dbuf = BytesMut::zeroed(body_data_len as usize);
    if body_data_len > 0 {
        let _ = reader.read_exact(&mut dbuf).await?;
    }
    let ev = Event {
        header,
        body: dbuf.freeze(),
    };
    Ok(ev)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct PartialVecWriter {
        max_write: usize,
        written: Vec<u8>,
    }

    impl PartialVecWriter {
        fn new(max_write: usize) -> Self {
            Self {
                max_write,
                written: Vec::new(),
            }
        }
    }

    impl AsyncWrite for PartialVecWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let n = self.max_write.min(buf.len());
            self.written.extend_from_slice(&buf[..n]);
            Poll::Ready(Ok(n))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<std::io::Result<usize>> {
            let mut remaining = self.max_write;
            let mut written = 0;
            for buf in bufs {
                if remaining == 0 {
                    break;
                }
                let n = remaining.min(buf.len());
                self.written.extend_from_slice(&buf[..n]);
                remaining -= n;
                written += n;
            }
            Poll::Ready(Ok(written))
        }

        fn is_write_vectored(&self) -> bool {
            true
        }
    }

    fn expected_bytes(flag_len: u32, stream_id: u32, body: &[u8]) -> Vec<u8> {
        let mut expected = Vec::with_capacity(EVENT_HEADER_LEN + body.len());
        expected.extend_from_slice(&flag_len.to_le_bytes());
        expected.extend_from_slice(&stream_id.to_le_bytes());
        expected.extend_from_slice(body);
        expected
    }

    #[tokio::test]
    async fn write_event_writes_empty_body_header() {
        let ev = new_ping_event();
        let mut writer = PartialVecWriter::new(3);
        write_event(&mut writer, ev).await.unwrap();
        assert_eq!(
            writer.written,
            expected_bytes(get_flag_len(0, FLAG_PING), 0, &[])
        );
    }

    #[tokio::test]
    async fn write_event_handles_partial_vectored_writes() {
        let body = Bytes::from_static(b"hello world");
        let ev = new_data_event(42, body.clone());
        let mut writer = PartialVecWriter::new(3);
        write_event(&mut writer, ev).await.unwrap();
        assert_eq!(
            writer.written,
            expected_bytes(get_flag_len(body.len() as u32, FLAG_DATA), 42, &body)
        );
    }
}
