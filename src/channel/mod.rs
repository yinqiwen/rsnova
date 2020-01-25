mod direct;

use std::error::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;

pub trait ChannelStream {
    fn split(
        &mut self,
    ) -> (
        Box<dyn AsyncRead + Send + Unpin + '_>,
        Box<dyn AsyncWrite + Send + Unpin + '_>,
    );
    fn close(&self) -> std::io::Result<()>;
}

pub async fn get_channel_stream(
    channel: String,
    addr: String,
) -> Result<Box<dyn ChannelStream + Send>, Box<dyn Error>> {
    let stream = direct::get_direct_stream(addr).await?;
    Ok(stream)
}
