mod direct;
mod rmux;
mod routine;
//mod ws;

use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;

pub use self::routine::routine_channels;

pub trait ChannelStream {
    fn split(
        &mut self,
    ) -> (
        Box<dyn AsyncRead + Send + Unpin + '_>,
        Box<dyn AsyncWrite + Send + Unpin + '_>,
    );
    fn close(&mut self) -> std::io::Result<()>;
}

pub async fn get_channel_stream(
    channel: String,
    addr: String,
) -> Result<Box<dyn ChannelStream + Send>, std::io::Error> {
    //irect::get_direct_stream(addr).await
    if channel == "direct" {
        direct::get_direct_stream(addr).await
    } else {
        rmux::get_rmux_stream(channel.as_str(), addr).await
    }
}
