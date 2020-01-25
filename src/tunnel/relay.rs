use futures::future;
use futures::future::join;
use futures::future::FutureExt;
use std::error::Error;
use tokio::io;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

pub async fn relay<'a, R, W, A, B>(
    tunnel_id: u32,
    local_reader: &'a mut A,
    local_writer: &'a mut B,
    remote_reader: &'a mut R,
    remote_writer: &'a mut W,
) -> Result<(), Box<dyn Error>>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
    A: AsyncRead + Unpin + ?Sized,
    B: AsyncWrite + Unpin + ?Sized,
{
    let client_to_server = async {
        io::copy(local_reader, remote_writer).await;
        error!("[{}]Close client_to_server", tunnel_id);
        remote_writer.shutdown().await;
        //()
    };
    let server_to_client = async {
        io::copy(remote_reader, local_writer).await;
        error!("[{}]Close server_to_client", tunnel_id);
        local_writer.shutdown().await;
        //()
    };
    join(client_to_server, server_to_client).await;
    Ok(())
}
