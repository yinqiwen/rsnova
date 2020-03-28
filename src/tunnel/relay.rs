use crate::channel::get_channel_stream;
use crate::config::TunnelConfig;
use crate::rmux::get_channel_session_size;
use crate::utils::{make_error, relay_buf_copy, RelayState};

use futures::future::join;
use std::error::Error;
use std::net::Shutdown;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::delay_for;

pub async fn relay_connection(
    tunnel_id: u32,
    mut inbound: TcpStream,
    cfg: &TunnelConfig,
    target: String,
    relay_buf: Vec<u8>,
) -> Result<(), Box<dyn Error>> {
    let (mut ri, mut wi) = inbound.split();
    //let mut ri = tokio::io::BufReader::new(ri);
    //let mut wi = tokio::io::BufWriter::new(wi);
    let _ = relay_stream(tunnel_id, &mut ri, &mut wi, target, cfg, relay_buf).await;
    let _ = inbound.shutdown(Shutdown::Both);
    Ok(())
}

pub async fn relay_stream<'a, A, B>(
    tunnel_id: u32,
    local_reader: &'a mut A,
    local_writer: &'a mut B,
    target: String,
    cfg: &TunnelConfig,
    relay_buf: Vec<u8>,
) -> Result<(), Box<dyn Error>>
where
    A: AsyncRead + Unpin + ?Sized,
    B: AsyncWrite + Unpin + ?Sized,
{
    let mut channel = String::new();
    for pac in cfg.pac.iter() {
        if pac.is_match(target.as_str()) {
            channel = String::from(pac.channel.as_str());
            if channel.as_str() != "direct" && get_channel_session_size(channel.as_str()) == 0 {
                continue;
            }
            break;
        }
    }
    if channel.is_empty() {
        return Err(make_error("no valid channel found."));
    }

    let remote_target = String::from(target.as_str());
    let mut remote = get_channel_stream(channel, target).await?;
    {
        let (mut ro, mut wo) = remote.split();
        if !relay_buf.is_empty() {
            wo.write_all(&relay_buf[..]).await?;
        }
        relay(
            tunnel_id,
            local_reader,
            local_writer,
            &mut ro,
            &mut wo,
            cfg.relay_buf_size(),
        )
        .await?;
    }
    let _ = remote.close();
    info!("[{}][{}]Stream close", tunnel_id, remote_target);
    Ok(())
}

pub async fn relay<'a, R, W, A, B>(
    tunnel_id: u32,
    local_reader: &'a mut A,
    local_writer: &'a mut B,
    remote_reader: &'a mut R,
    remote_writer: &'a mut W,
    relay_buf_size: usize,
) -> Result<(), Box<dyn Error>>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
    A: AsyncRead + Unpin + ?Sized,
    B: AsyncWrite + Unpin + ?Sized,
{
    let client_to_server_state = Arc::new(Mutex::new(RelayState::new()));
    let server_to_client_state = Arc::new(Mutex::new(RelayState::new()));
    let client_to_server = async {
        //let _ = buf_copy(local_reader, remote_writer, Box::new([0; RELAY_BUF_SIZE])).await;
        {
            let _ = relay_buf_copy(
                local_reader,
                remote_writer,
                vec![0; relay_buf_size],
                client_to_server_state.clone(),
            )
            .await;
            info!("[{}]Stream close client_to_server", tunnel_id);
        }
        let _ = remote_writer.shutdown().await;
        if !server_to_client_state.clone().lock().unwrap().is_closed() {
            delay_for(Duration::from_secs(5)).await;
            server_to_client_state.clone().lock().unwrap().close();
        }
    };

    let server_to_client = async {
        //let _ = buf_copy(remote_reader, local_writer, Box::new([0; RELAY_BUF_SIZE])).await;
        {
            let _ = relay_buf_copy(
                remote_reader,
                local_writer,
                vec![0; relay_buf_size],
                server_to_client_state.clone(),
            )
            .await;
            info!("[{}]Stream close server_to_client", tunnel_id);
        }
        let _ = local_writer.shutdown().await;
        if !client_to_server_state.clone().lock().unwrap().is_closed() {
            delay_for(Duration::from_secs(5)).await;
            client_to_server_state.clone().lock().unwrap().close();
        }
    };
    join(client_to_server, server_to_client).await;
    Ok(())
}
