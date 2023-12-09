use anyhow::{anyhow, Result};
use bincode::{Decode, Encode};
use futures::future::try_join;
use std::time::Duration;
use tokio::io::{self, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::mux::event::{self, OpenStreamEvent};

pub struct Stream<'a, LR, LW, RR, RW> {
    local_reader: &'a mut LR,
    local_writer: &'a mut LW,
    remote_reader: &'a mut RR,
    remote_writer: &'a mut RW,
}

async fn timeout_copy_impl<R: AsyncReadExt + Unpin, W: AsyncWriteExt + Unpin>(
    r: &mut R,
    w: &mut W,
    timeout_duration: std::time::Duration,
) -> Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let n = timeout(timeout_duration, r.read(&mut buf)).await??;
        if n == 0 {
            break;
        }
        w.write_all(&buf[0..n]).await?;
    }
    Ok(())
}
async fn timeout_copy<R: AsyncReadExt + Unpin, W: AsyncWriteExt + Unpin>(
    r: &mut R,
    w: &mut W,
    timeout_duration: std::time::Duration,
) -> Result<()> {
    let result = timeout_copy_impl(r, w, timeout_duration).await;
    w.shutdown().await?;
    result
}

impl<'a, LR, LW, RR, RW> Stream<'a, LR, LW, RR, RW>
where
    LR: AsyncReadExt + Unpin,
    LW: AsyncWriteExt + Unpin,
    RR: AsyncReadExt + Unpin,
    RW: AsyncWriteExt + Unpin,
{
    pub fn new(lr: &'a mut LR, lw: &'a mut LW, rr: &'a mut RR, rw: &'a mut RW) -> Self {
        Self {
            local_reader: lr,
            local_writer: lw,
            remote_reader: rr,
            remote_writer: rw,
        }
    }

    pub async fn transfer(&mut self) -> Result<()> {
        let timeout_secs = Duration::from_secs(5);
        let client_to_server = timeout_copy(
            &mut self.local_reader,
            &mut self.remote_writer,
            timeout_secs,
        );
        let server_to_client = timeout_copy(
            &mut self.remote_reader,
            &mut self.local_writer,
            timeout_secs,
        );
        try_join(client_to_server, server_to_client).await?;
        Ok(())
    }
}

pub async fn handle_server_stream<'a, LR: AsyncReadExt + Unpin, LW: AsyncWriteExt + Unpin>(
    mut lr: &'a mut LR,
    mut lw: &'a mut LW,
) -> Result<()> {
    let timeout_secs = Duration::from_secs(5);
    match timeout(timeout_secs, event::read_event(&mut lr)).await? {
        Err(e) => match e.kind() {
            std::io::ErrorKind::UnexpectedEof => Ok(()),
            _ => Err(anyhow::Error::new(e)),
        },
        Ok(ev) => {
            if ev.header.flags() != event::FLAG_OPEN {
                return Err(anyhow!("unexpected flag:{}", ev.header.flags()));
            }
            let config = bincode::config::standard();
            let (open_event, len): (OpenStreamEvent, usize) =
                bincode::decode_from_slice(&ev.body[..], config)?;
            tracing::info!("[{}]recv open event:{:?}", ev.header.stream_id, open_event);
            let mut remote_stream = timeout(
                timeout_secs,
                tokio::net::TcpStream::connect(&open_event.addr),
            )
            .await??;
            let (mut remote_receiver, mut remote_sender) = remote_stream.split();
            let mut stream: Stream<
                LR,
                LW,
                tokio::net::tcp::ReadHalf<'_>,
                tokio::net::tcp::WriteHalf<'_>,
            > = Stream::new(&mut lr, &mut lw, &mut remote_receiver, &mut remote_sender);
            stream.transfer().await
        }
    }
}
