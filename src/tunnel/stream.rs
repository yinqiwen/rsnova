use anyhow::{anyhow, Result};
use futures::future::try_join;
use once_cell::sync::Lazy;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use crate::mux::event::{self, OpenStreamEvent};
use crate::tunnel::CHECK_TIMEOUT_SECS;
use crate::tunnel::DEFAULT_TIMEOUT_SECS;

static STREAM_BUFFER_POOL: Lazy<lockfree_object_pool::LinearObjectPool<[u8; 8192]>> =
    Lazy::new(|| {
        lockfree_object_pool::LinearObjectPool::<[u8; 8192]>::new(
            || [0u8; 8192],
            |_| {
                //*v = 0;
            },
        )
    });

struct TransferState {
    abort: AtomicBool,
    io_active_timestamp_secs: AtomicU64,
}

impl TransferState {
    fn new() -> Self {
        Self {
            abort: AtomicBool::new(false),
            io_active_timestamp_secs: AtomicU64::new(0),
        }
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
    //let mut buf = [0u8; 8192];
    let mut buf = STREAM_BUFFER_POOL.pull();
    let check_timeout_secs = Duration::from_secs(CHECK_TIMEOUT_SECS);
    loop {
        if state.abort.load(SeqCst) {
            return Err(anyhow!("abort"));
        }
        match timeout(check_timeout_secs, r.read(buf.as_mut())).await {
            Err(_) => {
                let now_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                if now_secs > (state.io_active_timestamp_secs.load(SeqCst) + timeout_sec) {
                    return Err(anyhow!(format!(
                        "timeout after inactive {}secs",
                        now_secs - state.io_active_timestamp_secs.load(SeqCst)
                    )));
                } else {
                    continue;
                }
            }
            Ok(Ok(n)) => {
                if n == 0 {
                    break;
                };
                state.io_active_timestamp_secs.store(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                    SeqCst,
                );
                if let Err(ex) = w.write_all(&buf[0..n]).await {
                    state.abort.store(true, SeqCst);
                    return Err(ex.into());
                }
            }
            Ok(Err(e)) => {
                state.abort.store(true, SeqCst);
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

    pub async fn transfer(&mut self) -> Result<()> {
        let state = Arc::new(TransferState::new());
        let client_to_server = timeout_copy(
            &mut self.local_reader,
            &mut self.remote_writer,
            DEFAULT_TIMEOUT_SECS,
            state.clone(),
        );
        let server_to_client = timeout_copy(
            &mut self.remote_reader,
            &mut self.local_writer,
            DEFAULT_TIMEOUT_SECS,
            state.clone(),
        );
        try_join(client_to_server, server_to_client).await?;
        Ok(())
    }
}

pub async fn handle_server_stream<'a, LR: AsyncReadExt + Unpin, LW: AsyncWriteExt + Unpin>(
    mut lr: &'a mut LR,
    lw: &'a mut LW,
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
            > = Stream::new(lr, lw, &mut remote_receiver, &mut remote_sender);
            stream.transfer().await
        }
    }
}
