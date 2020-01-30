use bytes::Bytes;
use bytes::BytesMut;
use std::error::Error;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

pub async fn read_until_separator(
    stream: &mut TcpStream,
    separator: &str,
) -> Result<(Bytes, Bytes), Box<dyn Error>> {
    let mut buf = BytesMut::with_capacity(1024);

    loop {
        buf.reserve(1024);
        stream.read_buf(&mut buf).await?;
        if let Some(pos) = twoway::find_bytes(&buf, separator.as_bytes()) {
            let wsize = separator.len();
            let body = buf.split_off(pos + wsize);
            return Ok((buf.freeze(), body.freeze()));
        }
    }
}
