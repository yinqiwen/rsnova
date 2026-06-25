use anyhow::{Result, anyhow};

use futures::future::try_join;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use crate::mux::event::{self, OpenStreamEvent};
use crate::tunnel::CHECK_TIMEOUT_SECS;
use crate::tunnel::DEFAULT_TIMEOUT_SECS;
use crate::utils::UdpClientStream;

struct TransferState {
    abort: AtomicBool,
    start: Instant,
    last_active_millis: AtomicU64,
}

impl TransferState {
    fn new() -> Self {
        Self {
            abort: AtomicBool::new(false),
            start: Instant::now(),
            last_active_millis: AtomicU64::new(0),
        }
    }
    fn touch(&self) {
        self.last_active_millis
            .store(self.start.elapsed().as_millis() as u64, Relaxed);
    }
    fn idle_snapshot(&self) -> (u64, u64) {
        let now_millis = self.start.elapsed().as_millis() as u64;
        let last_active_millis = self.last_active_millis.load(Relaxed);
        (
            now_millis.saturating_sub(last_active_millis),
            last_active_millis,
        )
    }
}

pub struct Stream<'a, LR, LW, RR, RW> {
    local_reader: &'a mut LR,
    local_writer: &'a mut LW,
    remote_reader: &'a mut RR,
    remote_writer: &'a mut RW,
}

async fn timeout_copy_impl<R: AsyncReadExt + Unpin, W: AsyncWriteExt + Unpin>(
    r: &mut R,
    w: &mut W,
    timeout_sec: u64,
    state: Arc<TransferState>,
) -> Result<()> {
    let mut buf = [0u8; 8192];

    let check_timeout_secs = Duration::from_secs(CHECK_TIMEOUT_SECS);
    state.touch();
    loop {
        if state.abort.load(Relaxed) {
            metrics::counter!("mux.stream.close.abort").increment(1);
            return Err(anyhow!("abort"));
        }
        match timeout(check_timeout_secs, r.read(&mut buf)).await {
            Err(_) => {
                let (idle_millis, last_active_millis) = state.idle_snapshot();
                if idle_millis >= timeout_sec.saturating_mul(1000) {
                    metrics::counter!("mux.stream.close.idle_timeout").increment(1);
                    return Err(anyhow!(format!(
                        "timeout after inactive {}ms, last active at +{}ms",
                        idle_millis, last_active_millis
                    )));
                } else {
                    continue;
                }
            }
            Ok(Ok(n)) => {
                state.touch();
                if n == 0 {
                    metrics::counter!("mux.stream.close.eof").increment(1);
                    break;
                };
                if let Err(ex) = w.write_all(&buf[0..n]).await {
                    state.abort.store(true, Relaxed);
                    metrics::counter!("mux.stream.close.write_error").increment(1);
                    return Err(ex.into());
                }
            }
            Ok(Err(e)) => {
                state.abort.store(true, Relaxed);
                metrics::counter!("mux.stream.close.read_error").increment(1);
                return Err(e.into());
            }
        }
    }
    Ok(())
}
async fn timeout_copy<R: AsyncReadExt + Unpin, W: AsyncWriteExt + Unpin>(
    r: &mut R,
    w: &mut W,
    timeout_sec: u64,
    state: Arc<TransferState>,
) -> Result<()> {
    let result = timeout_copy_impl(r, w, timeout_sec, state).await;
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

    pub async fn transfer(&mut self, idle_timeout_secs: usize) -> Result<()> {
        let state = Arc::new(TransferState::new());
        let client_to_server = timeout_copy(
            &mut self.local_reader,
            &mut self.remote_writer,
            idle_timeout_secs as u64,
            state.clone(),
        );
        let server_to_client = timeout_copy(
            &mut self.remote_reader,
            &mut self.local_writer,
            idle_timeout_secs as u64,
            state.clone(),
        );
        try_join(client_to_server, server_to_client).await?;
        Ok(())
    }
}

pub async fn handle_server_stream<'a, LR: AsyncReadExt + Unpin, LW: AsyncWriteExt + Unpin>(
    mut lr: &'a mut LR,
    lw: &'a mut LW,
    idle_timeout_secs: usize,
) -> Result<()> {
    let timeout_secs = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
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
            let (open_event, _len): (OpenStreamEvent, usize) =
                bincode::decode_from_slice(ev.body.as_ref(), config)?;
            tracing::info!("[{}]recv open event:{:?}", ev.header.stream_id, open_event);
            if open_event.proto == "udp" {
                let udp_socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
                udp_socket.connect(&open_event.addr).await?;
                let udp_stream = UdpClientStream::new(udp_socket);
                let (mut remote_receiver, mut remote_sender) = tokio::io::split(udp_stream);
                let mut stream = Stream::new(lr, lw, &mut remote_receiver, &mut remote_sender);
                stream.transfer(idle_timeout_secs).await
            } else {
                let mut remote_stream = timeout(
                    timeout_secs,
                    tokio::net::TcpStream::connect(&open_event.addr),
                )
                .await??;
                let (mut remote_receiver, mut remote_sender) = remote_stream.split();
                let mut stream = Stream::new(lr, lw, &mut remote_receiver, &mut remote_sender);
                stream.transfer(idle_timeout_secs).await
            }
        }
    }
}
