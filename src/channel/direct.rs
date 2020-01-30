use super::ChannelStream;

use std::net::Shutdown;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::net::TcpStream;

struct DirectChannelStream {
    pub conn: TcpStream,
}

impl DirectChannelStream {
    pub fn new(s: TcpStream) -> Self {
        Self { conn: s }
    }
}

impl ChannelStream for DirectChannelStream {
    fn split(
        &mut self,
    ) -> (
        Box<dyn AsyncRead + Send + Unpin + '_>,
        Box<dyn AsyncWrite + Send + Unpin + '_>,
    ) {
        let (r, w) = self.conn.split();
        (Box::new(r), Box::new(w))
    }
    fn close(&mut self) -> std::io::Result<()> {
        self.conn.shutdown(Shutdown::Both)
    }
}

pub async fn get_direct_stream(
    addr: String,
) -> Result<Box<dyn ChannelStream + Send>, std::io::Error> {
    let conn = TcpStream::connect(&addr);
    let dur = std::time::Duration::from_secs(3);
    let s = tokio::time::timeout(dur, conn).await?;

    match s {
        Ok(c) => Ok(Box::new(DirectChannelStream::new(c))),
        Err(e) => Err(e),
    }
}
