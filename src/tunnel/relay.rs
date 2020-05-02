use crate::channel::get_channel_stream;
use crate::config::TunnelConfig;
use crate::rmux::get_channel_session_size;
use crate::utils::{make_error, relay_buf_copy, RelayState};

use futures::future::join3;
use std::error::Error;
use std::net::Shutdown;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time;
use tokio::time::delay_for;

// static RELAYS: AtomicU32 = AtomicU32::new(0);

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
    // RELAYS.fetch_add(1, Ordering::SeqCst);
    // info!(
    //     "[{}][{}]Stream open with curent relay:{}",
    //     tunnel_id,
    //     remote_target,
    //     RELAYS.load(Ordering::SeqCst)
    // );
    let mut remote = match get_channel_stream(channel, target).await {
        Ok(s) => s,
        Err(e) => {
            //RELAYS.fetch_sub(1, Ordering::SeqCst);
            return Err(make_error(&e.to_string()));
        }
    };
    {
        let (mut ro, mut wo) = remote.split();
        let no_relay = !relay_buf.is_empty() && wo.write_all(&relay_buf[..]).await.is_err();
        if !no_relay {
            let _ = relay(
                tunnel_id,
                local_reader,
                local_writer,
                &mut ro,
                &mut wo,
                cfg.relay_buf_size(),
            )
            .await;
        }
    }
    let _ = remote.close();
    // RELAYS.fetch_sub(1, Ordering::SeqCst);
    // info!(
    //     "[{}][{}]Stream close with curent relay:{}",
    //     tunnel_id,
    //     remote_target,
    //     RELAYS.load(Ordering::SeqCst)
    // );
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
    let c2s_state = Arc::new(Mutex::new(RelayState::new()));
    let s2c_state = Arc::new(Mutex::new(RelayState::new()));

    let c2s_state_c2s = c2s_state.clone();
    let s2c_state_c2s = s2c_state.clone();
    let client_to_server = async {
        //let _ = buf_copy(local_reader, remote_writer, Box::new([0; RELAY_BUF_SIZE])).await;
        {
            let _ = relay_buf_copy(
                local_reader,
                remote_writer,
                vec![0; relay_buf_size],
                c2s_state_c2s.clone(),
            )
            .await;
            info!("[{}]Stream close client_to_server", tunnel_id);
        }
        c2s_state_c2s.lock().unwrap().close();
        let _ = remote_writer.shutdown().await;
        if !s2c_state_c2s.lock().unwrap().is_closed() {
            delay_for(Duration::from_secs(5)).await;
            s2c_state_c2s.lock().unwrap().close();
        }
    };

    let c2s_state_s2c = c2s_state.clone();
    let s2c_state_s2c = s2c_state.clone();
    let server_to_client = async {
        //let _ = buf_copy(remote_reader, local_writer, Box::new([0; RELAY_BUF_SIZE])).await;
        {
            let _ = relay_buf_copy(
                remote_reader,
                local_writer,
                vec![0; relay_buf_size],
                s2c_state_s2c.clone(),
            )
            .await;
            info!("[{}]Stream close server_to_client", tunnel_id);
        }
        s2c_state_s2c.lock().unwrap().close();
        let _ = local_writer.shutdown().await;
        if !c2s_state_s2c.lock().unwrap().is_closed() {
            delay_for(Duration::from_secs(5)).await;
            c2s_state_s2c.lock().unwrap().close();
        }
    };

    let check_timeout = async {
        let mut interval = time::interval(Duration::from_secs(1));
        let max_wait_secs = 30;
        while !c2s_state.lock().unwrap().is_closed() || !s2c_state.lock().unwrap().is_closed() {
            interval.tick().await;
            if c2s_state.lock().unwrap().pending_elapsed().as_secs() > max_wait_secs
                && s2c_state.lock().unwrap().pending_elapsed().as_secs() > max_wait_secs
            {
                c2s_state.lock().unwrap().close();
                s2c_state.lock().unwrap().close();
                return;
            }
        }
    };

    join3(client_to_server, server_to_client, check_timeout).await;
    Ok(())
}
