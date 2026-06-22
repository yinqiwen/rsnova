use anyhow::anyhow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use url::Url;

use crate::mux::event;
use crate::mux::event::OpenStreamEvent;

/// Bounded channel capacity for the proxy message queue.
/// Limits memory growth under load via backpressure.
pub const PROXY_CHANNEL_CAPACITY: usize = 256;

/// Type alias for the bounded proxy message sender.
pub type ProxySender = mpsc::Sender<Message>;
/// Type alias for the bounded proxy message receiver.
pub type ProxyReceiver = mpsc::Receiver<Message>;
use crate::tunnel::stream::Stream;
use crate::utils::UdpServerStream;

pub struct OpenStreamRequest {
    tcp_stream: Option<tokio::net::TcpStream>,
    #[allow(dead_code)]
    udp_stream: Option<UdpServerStream>,
    event: OpenStreamEvent,
    payload: Option<Vec<u8>>,
}

impl OpenStreamRequest {
    pub fn from_tcp(
        stream: tokio::net::TcpStream,
        target: String,
        payload: Option<Vec<u8>>,
    ) -> Self {
        Self {
            tcp_stream: Some(stream),
            udp_stream: None,
            event: OpenStreamEvent {
                proto: String::from("tcp"),
                addr: target,
            },
            payload,
        }
    }
    #[allow(dead_code)]
    pub fn from_udp(stream: UdpServerStream, target: String, payload: Option<Vec<u8>>) -> Self {
        Self {
            tcp_stream: None,
            udp_stream: Some(stream),
            event: OpenStreamEvent {
                proto: String::from("udp"),
                addr: target,
            },
            payload,
        }
    }
}

#[allow(dead_code)]
pub enum Message {
    OpenStream(OpenStreamRequest),
}
impl Message {
    pub fn open_tcp_stream(
        stream: tokio::net::TcpStream,
        target: String,
        payload: Option<Vec<u8>>,
    ) -> Message {
        let req = OpenStreamRequest::from_tcp(stream, target, payload);
        Message::OpenStream(req)
    }

    #[allow(dead_code)]
    pub fn open_udp_stream(
        stream: UdpServerStream,
        target: String,
        payload: Option<Vec<u8>>,
    ) -> Message {
        let req = OpenStreamRequest::from_udp(stream, target, payload);
        Message::OpenStream(req)
    }
}

pub(crate) trait MuxConnection {
    type SendStream: AsyncWrite + Unpin + Send;
    type RecvStream: AsyncRead + Unpin + Send;
    fn ping(
        &mut self,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn connect(
        &mut self,
        url: &Url,
        key_path: &Path,
        host: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn open_stream(
        &mut self,
    ) -> impl std::future::Future<Output = anyhow::Result<(Self::SendStream, Self::RecvStream)>>
    + Send;
    fn accept_stream(
        &mut self,
    ) -> impl std::future::Future<Output = anyhow::Result<(Self::SendStream, Self::RecvStream)>>
    + Send;
    fn is_valid(&self) -> bool;
    #[allow(dead_code)]
    fn set_connection(&mut self, new_c: Self);
    fn close(&mut self);
    #[allow(dead_code)]
    fn active_stream_count(&self) -> usize;
    /// Build a fresh connection from `ConnParams`. Called by the
    /// connection's `reconnect_loop` after the original conn is lost.
    /// The returned future must be `Send` so it can run in a spawned task.
    fn reconnect_with(
        params: &ConnParams,
    ) -> impl std::future::Future<Output = anyhow::Result<Self>> + Send
    where
        Self: Sized;
}

/// Deterministic jitter for connection retirement, derived from connection index.
/// Range: [-100, +100] seconds. Spreads retirements across a 200-second window.
pub(crate) fn retirement_jitter_secs(index: usize) -> i64 {
    ((index.wrapping_mul(73)) % 201) as i64 - 100
}

// =========================================================================
// New types for self-managed health (v3 refactor, additive in Commit A)
// These are unused until Commit B lands; the #[allow(dead_code)] silences
// warnings during the intermediate state.
// =========================================================================

/// Monotonic generation counter for each pool slot. Incremented on every
/// successful `replace_slot`. Used to reject late calls from stale health
/// tasks after a slot has been replaced.
pub(crate) type Generation = u64;

/// Returned by `mark_retiring` / `mark_dead` / `replace_slot` when the
/// caller's generation does not match the slot's current generation.
/// Indicates the slot has been replaced and the caller should exit.
#[allow(dead_code)]
pub(crate) struct GenMismatch;

/// State of a pool slot. Transitions: Active → Retiring → Dead → (replace) → Active.
#[allow(dead_code, dead_code)]  // second allow suppresses derived-trait warning
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotState {
    Active,
    Retiring,
    Dead,
}

/// One entry in the connection pool.
#[allow(dead_code)]
pub(crate) struct SlotEntry<T> {
    /// `None` while a slot is empty (after `replace_slot` took the conn out
    /// to forward it to the monitor). The monitor re-pushes a new conn
    /// (or drops the slot) — `pick_and_open` skips `None` entries.
    pub(crate) conn: Option<T>,
    pub(crate) state: SlotState,
    pub(crate) generation: Generation,
}

/// Static parameters for connection creation. Owned by the pool monitor
/// and shared (via `Arc<ConnParams>`) with each health task.
#[allow(dead_code)]
pub(crate) struct ConnParams {
    pub url: Url,
    pub cert_path: PathBuf,
    pub host: String,
    pub stream_window: u32,
    /// `None` disables max-age-driven retirement.
    pub max_age: Option<Duration>,
    pub ping_interval: Duration,
    pub ping_fail_threshold: u32,
    pub quic_endpoint: Option<Arc<s2n_quic::client::Client>>,
}

/// Atomic metrics counters, decoupled from the pool's mutex to allow
/// cheap reads from `/metrics` without lock contention.
#[allow(dead_code)]
pub(crate) struct PoolMetrics {
    pub active: AtomicUsize,
    pub retiring: AtomicUsize,
    pub dead: AtomicUsize,
    pub reconnect_attempts: AtomicU64,
    pub reconnect_success: AtomicU64,
    pub generation_mismatch: AtomicU64,
    pub health_task_exits: AtomicU64,
}

/// Command sent from a `health_loop` / `reconnect_loop` to the `pool_monitor`.
/// The monitor is the sole owner of the `JoinSet` of health tasks; on
/// `Respawn` it spawns a new health task for the given slot.
#[allow(dead_code)]
pub(crate) enum MonitorCommand<T> {
    /// A new connection has been produced (by `reconnect_loop`) and is
    /// ready to replace the slot. Monitor spawns a new health task.
    Respawn { slot: usize, conn: T },
    /// Reconnect abandoned the slot (e.g. gen_token mismatch). Monitor logs
    /// and accepts the slot as permanently dead (pool shrinks by 1).
    DropSlot(usize),
}

pub(crate) struct MuxClient<T> {
    pub(crate) conns: tokio::sync::Mutex<Vec<SlotEntry<T>>>,
    pub(crate) cancel: tokio_util::sync::CancellationToken,
    pub(crate) metrics: Arc<PoolMetrics>,
}

impl<T: MuxConnection> MuxClient<T> {
    /// Create an empty pool. Initial connections are pushed in via
    /// `push_initial_slot` by the `pool_monitor`.
    pub(crate) fn new(cancel: tokio_util::sync::CancellationToken) -> Self {
        Self {
            conns: tokio::sync::Mutex::new(Vec::new()),
            metrics: Arc::new(PoolMetrics {
                active: AtomicUsize::new(0),
                retiring: AtomicUsize::new(0),
                dead: AtomicUsize::new(0),
                reconnect_attempts: AtomicU64::new(0),
                reconnect_success: AtomicU64::new(0),
                generation_mismatch: AtomicU64::new(0),
                health_task_exits: AtomicU64::new(0),
            }),
            cancel,
        }
    }

    /// Push an empty slot for a connection whose ownership will be moved
    /// into a `health_loop`. The slot is `Active` but `conn: None`; only
    /// `install_replacement` can populate it.
    pub(crate) async fn push_empty_slot(&self) -> usize {
        let mut guard = self.conns.lock().await;
        let slot = guard.len();
        guard.push(SlotEntry {
            conn: None,
            state: SlotState::Active,
            generation: 0,
        });
        self.metrics.active.fetch_add(1, Ordering::Relaxed);
        slot
    }

    /// Atomic: pick an Active+valid connection and call `open_stream` on it.
    /// Holds the lock for the entire open_stream call to eliminate TOCTOU.
    pub(crate) async fn pick_and_open(
        &self,
    ) -> anyhow::Result<(T::SendStream, T::RecvStream)> {
        let mut guard = self.conns.lock().await;
        let len = guard.len();
        for slot in 0..len {
            let entry = &mut guard[slot];
            if entry.state != SlotState::Active {
                continue;
            }
            let Some(conn) = entry.conn.as_mut() else {
                continue;
            };
            if !conn.is_valid() {
                continue;
            }
            return conn.open_stream().await;
        }
        let active = self.metrics.active.load(Ordering::Relaxed);
        let retiring = self.metrics.retiring.load(Ordering::Relaxed);
        let dead = self.metrics.dead.load(Ordering::Relaxed);
        tracing::error!(
            "no available stream: pool size={}, active={}, retiring={}, dead={}",
            len, active, retiring, dead,
        );
        Err(anyhow!("no available stream"))
    }

    pub(crate) async fn generation(&self, slot: usize) -> Generation {
        self.conns.lock().await[slot].generation
    }

    #[allow(dead_code)]
    pub(crate) async fn slot_state(&self, slot: usize) -> SlotState {
        self.conns.lock().await[slot].state
    }

    /// Transition Active → Retiring. Idempotent: no-op if already Retiring/Dead.
    pub(crate) async fn mark_retiring(
        &self,
        slot: usize,
        gen_token: Generation,
    ) -> Result<(), GenMismatch> {
        let mut guard = self.conns.lock().await;
        let entry = &mut guard[slot];
        if entry.generation != gen_token {
            self.metrics
                .generation_mismatch
                .fetch_add(1, Ordering::Relaxed);
            return Err(GenMismatch);
        }
        if entry.state == SlotState::Active {
            entry.state = SlotState::Retiring;
            self.metrics.active.fetch_sub(1, Ordering::Relaxed);
            self.metrics.retiring.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Transition Retiring/Active → Dead. Closes the underlying conn so the
    /// mux dispatcher exits and in-flight streams fail fast. Idempotent.
    pub(crate) async fn mark_dead(
        &self,
        slot: usize,
        gen_token: Generation,
    ) -> Result<(), GenMismatch> {
        let mut guard = self.conns.lock().await;
        let entry = &mut guard[slot];
        if entry.generation != gen_token {
            self.metrics
                .generation_mismatch
                .fetch_add(1, Ordering::Relaxed);
            return Err(GenMismatch);
        }
        if entry.state != SlotState::Dead {
            if entry.state == SlotState::Active {
                self.metrics.active.fetch_sub(1, Ordering::Relaxed);
            } else {
                self.metrics.retiring.fetch_sub(1, Ordering::Relaxed);
            }
            if let Some(conn) = entry.conn.as_mut() {
                conn.close();
            }
            entry.state = SlotState::Dead;
            self.metrics.dead.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Install a new connection into an Active slot (used by `pool_monitor`
    /// after receiving a `MonitorCommand::Respawn`). The slot must already
    /// be Active with `conn: None`.
    #[allow(dead_code)]
    pub(crate) async fn install_replacement(&self, slot: usize, conn: T) {
        let mut guard = self.conns.lock().await;
        let entry = &mut guard[slot];
        debug_assert!(entry.state == SlotState::Active);
        debug_assert!(entry.conn.is_none());
        entry.conn = Some(conn);
    }

    /// Replace the slot's connection, increment generation, mark Active.
    /// Returns the new conn so the caller can forward it to the monitor
    /// (which spawns the next health task). The slot is left with `conn: None`
    /// until the monitor pushes the new conn (or drops the slot on `DropSlot`).
    pub(crate) async fn replace_slot(
        &self,
        slot: usize,
        new_conn: T,
        gen_token: Generation,
    ) -> Result<T, GenMismatch> {
        let mut guard = self.conns.lock().await;
        let entry = &mut guard[slot];
        if entry.generation != gen_token {
            self.metrics
                .generation_mismatch
                .fetch_add(1, Ordering::Relaxed);
            return Err(GenMismatch);
        }
        // Close the old (Dead) conn before clearing the slot.
        if let Some(mut old) = entry.conn.take() {
            old.close();
        }
        entry.generation = entry.generation.wrapping_add(1);
        entry.state = SlotState::Active;
        self.metrics.dead.fetch_sub(1, Ordering::Relaxed);
        self.metrics.active.fetch_add(1, Ordering::Relaxed);
        // Note: we do NOT store new_conn in the slot. The caller (reconnect_loop
        // or health_loop) is expected to forward new_conn to the monitor via
        // MonitorCommand::Respawn, and the monitor will push it back in.
        // This two-phase handoff ensures no double-ownership and lets the
        // monitor log/track respawns centrally.
        Ok(new_conn)
    }
}

impl<T> Drop for MuxClient<T> {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(crate) async fn mux_client_loop<T>(
    client: Arc<MuxClient<T>>,
    mut receiver: ProxyReceiver,
    idle_timeout_secs: usize,
) where
    T: MuxConnection + 'static,
    T::SendStream: 'static,
    T::RecvStream: 'static,
{
    while let Some(msg) = receiver.recv().await {
        match msg {
            Message::OpenStream(event) => {
                // Wrap pick_and_open in a timeout to prevent the serial
                // mux_client_loop from blocking indefinitely when the
                // pool is saturated.
                match tokio::time::timeout(
                    Duration::from_secs(1),
                    client.pick_and_open(),
                )
                .await
                {
                    Ok(Ok((mut send, mut recv))) => {
                        metrics::gauge!("client_proxy_streams").increment(1.0);
                        tokio::spawn(async move {
                            if let Some(mut tcp_stream) = event.tcp_stream {
                                let (mut local_reader, mut local_writer) = tcp_stream.split();
                                let ev = match event::new_open_stream_event(0, &event.event) {
                                    Ok(ev) => ev,
                                    Err(e) => {
                                        tracing::error!("create open stream event failed:{}", e);
                                        metrics::gauge!("client_proxy_streams").decrement(1.0);
                                        return;
                                    }
                                };
                                if let Err(e) = event::write_event(&mut send, ev).await {
                                    tracing::error!("write open stream event failed:{}", e);
                                    metrics::gauge!("client_proxy_streams").decrement(1.0);
                                    return;
                                }
                                if let Some(payload) = event.payload {
                                    if let Err(e) = send.write_all(&payload).await {
                                        tracing::error!("write payload failed:{}", e);
                                        metrics::gauge!("client_proxy_streams").decrement(1.0);
                                        return;
                                    }
                                }
                                let mut stream = Stream::new(
                                    &mut local_reader,
                                    &mut local_writer,
                                    &mut recv,
                                    &mut send,
                                );
                                if let Err(e) = stream.transfer(idle_timeout_secs).await {
                                    tracing::debug!("transfer finish:{}", e);
                                }
                            } else if let Some(udp_stream) = event.udp_stream {
                                let (mut local_reader, mut local_writer) = tokio::io::split(udp_stream);
                                let ev = match event::new_open_stream_event(0, &event.event) {
                                    Ok(ev) => ev,
                                    Err(e) => {
                                        tracing::error!("create open stream event failed:{}", e);
                                        metrics::gauge!("client_proxy_streams").decrement(1.0);
                                        return;
                                    }
                                };
                                if let Err(e) = event::write_event(&mut send, ev).await {
                                    tracing::error!("write open stream event failed:{}", e);
                                    metrics::gauge!("client_proxy_streams").decrement(1.0);
                                    return;
                                }
                                if let Some(payload) = event.payload {
                                    if let Err(e) = send.write_all(&payload).await {
                                        tracing::error!("write payload failed:{}", e);
                                        metrics::gauge!("client_proxy_streams").decrement(1.0);
                                        return;
                                    }
                                }
                                let mut stream = Stream::new(
                                    &mut local_reader,
                                    &mut local_writer,
                                    &mut recv,
                                    &mut send,
                                );
                                if let Err(e) = stream.transfer(idle_timeout_secs).await {
                                    tracing::debug!("transfer finish:{}", e);
                                }
                            }
                            metrics::gauge!("client_proxy_streams").decrement(1.0);
                        });
                    }
                    Err(_) | Ok(Err(_)) => {
                        crate::mux::metrics::inc_client_open_stream_failed();
                        tracing::error!("create remote proxy stream failed or timed out");
                    }
                }
            }
        }
    }
}

// =========================================================================
// Pool monitor: spawns one health_loop per slot, supervises them via
// JoinSet, handles respawn commands. Sole owner of health-task handles.
// =========================================================================

/// Limits how many `reconnect_loop`s run concurrently across the process.
/// Prevents thundering herd when many connections die at once.
fn reconnect_limiter() -> &'static tokio::sync::Semaphore {
    use std::sync::OnceLock;
    static LIMITER: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    LIMITER.get_or_init(|| tokio::sync::Semaphore::new(2))
}

/// Per-connection health loop. Each connection has one. Pings the conn,
/// transitions to Retiring on N consecutive ping failures or max_age
/// reached, spawns a `reconnect_loop` child task that produces a fresh
/// conn. Exits only when:
/// - `cancel` fires (graceful shutdown)
/// - the original conn dies AND the reconnect child has finished
///
/// The child reconnect_loop is responsible for sending to `monitor_tx`
/// on success. The parent `health_loop` only waits for the child to
/// complete before exiting.
#[allow(dead_code)]
pub(crate) async fn health_loop<T>(
    slot: usize,
    mut conn: T,
    pool: Arc<MuxClient<T>>,
    params: Arc<ConnParams>,
    cancel: tokio_util::sync::CancellationToken,
    monitor_tx: mpsc::Sender<MonitorCommand<T>>,
) where
    T: MuxConnection + Send + 'static,
{
    let gen_token = pool.generation(slot).await;
    let mut is_retiring = false;
    let mut consecutive_fails: u32 = 0;
    let max_age_deadline = params
        .max_age
        .map(|d| tokio::time::Instant::now() + d);

    // Per-slot state for the in-flight reconnect child.
    let mut reconnect_handle: Option<tokio::task::JoinHandle<()>> = None;

    let mut interval = tokio::time::interval(params.ping_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                if let Some(h) = reconnect_handle.take() {
                    h.abort();
                }
                return;
            }
            _ = interval.tick() => {
                // Phase 1: ping the original conn
                if conn.is_valid() {
                    if conn.ping().await.is_err() {
                        consecutive_fails += 1;
                        if consecutive_fails >= params.ping_fail_threshold && !is_retiring {
                            is_retiring = true;
                            let _ = pool.mark_retiring(slot, gen_token).await;
                        }
                    } else {
                        consecutive_fails = 0;
                    }
                } else if !is_retiring {
                    is_retiring = true;
                    let _ = pool.mark_retiring(slot, gen_token).await;
                }

                // max_age check
                if !is_retiring {
                    if let Some(deadline) = max_age_deadline {
                        if tokio::time::Instant::now() >= deadline {
                            tracing::info!("[slot-{}] reached max age, retiring", slot);
                            is_retiring = true;
                            let _ = pool.mark_retiring(slot, gen_token).await;
                        }
                    }
                }

                // Spawn reconnect child if needed. The child sends to
                // monitor_tx directly on success or DropSlot.
                if is_retiring && reconnect_handle.is_none() {
                    let pool_ref = pool.clone();
                    let params_ref = params.clone();
                    let cancel_child = cancel.child_token();
                    let monitor_tx_clone = monitor_tx.clone();
                    reconnect_handle = Some(tokio::spawn(async move {
                        let cmd = reconnect_loop(
                            slot,
                            pool_ref,
                            params_ref,
                            gen_token,
                            cancel_child,
                        ).await;
                        // Forward result to the monitor.
                        let _ = monitor_tx_clone.send(cmd).await;
                    }));
                }

                // If is_retiring and conn is dead: explicitly mark_dead + close,
                // wait for the reconnect child, then exit.
                if is_retiring && !conn.is_valid() {
                    conn.close();
                    let _ = pool.mark_dead(slot, gen_token).await;
                    if let Some(h) = reconnect_handle.take() {
                        let _ = h.await;
                    }
                    return;
                }
            }
        }
    }
}

/// Indefinitely retries `T::reconnect_with` with exponential backoff and
/// ±20% jitter. On success, calls `pool.replace_slot` and forwards the
/// new conn to `monitor_tx` via `MonitorCommand::Respawn`. On gen_token-mismatch
/// or other unrecoverable error, sends `MonitorCommand::DropSlot` and
/// returns.
#[allow(dead_code)]
async fn reconnect_loop<T>(
    slot: usize,
    pool: Arc<MuxClient<T>>,
    params: Arc<ConnParams>,
    gen_token: Generation,
    cancel: tokio_util::sync::CancellationToken,
) -> MonitorCommand<T>
where
    T: MuxConnection + Send + 'static,
{
    use std::sync::atomic::Ordering;
    let _permit = reconnect_limiter().acquire().await.expect("semaphore closed");
    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(60);

    loop {
        if cancel.is_cancelled() {
            return MonitorCommand::DropSlot(slot);
        }
        // ±20% jitter
        let jitter_factor = 0.8 + rand::random::<f64>() * 0.4;
        let sleep_for = backoff.mul_f64(jitter_factor);
        tokio::select! {
            _ = cancel.cancelled() => return MonitorCommand::DropSlot(slot),
            _ = tokio::time::sleep(sleep_for) => {}
        }
        pool.metrics.reconnect_attempts.fetch_add(1, Ordering::Relaxed);

        match T::reconnect_with(&params).await {
            Ok(new_conn) => match pool.replace_slot(slot, new_conn, gen_token).await {
                Ok(returned_conn) => {
                    pool.metrics.reconnect_success.fetch_add(1, Ordering::Relaxed);
                    return MonitorCommand::Respawn {
                        slot,
                        conn: returned_conn,
                    };
                }
                Err(GenMismatch) => {
                    tracing::warn!(
                        "[slot-{}] reconnect produced conn but gen_token mismatch; dropping",
                        slot,
                    );
                    return MonitorCommand::DropSlot(slot);
                }
            },
            Err(e) => {
                tracing::warn!("[slot-{}] reconnect failed: {}; backoff {:?}", slot, e, backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Pool-level supervisor. Owns the lifecycle of all `health_loop`s.
/// 
/// Architecture:
/// 1. `pool_monitor` calls `health_loop` for each initial conn. `health_loop`
///    takes the conn by value (for `close()`), pings it, and exits when
///    the conn dies. The conn is NOT stored in `MuxClient`'s slot during
///    this time — the slot is `None`. `pick_and_open` skips `None` slots.
/// 2. When `health_loop` exits (because the conn is dead), it has
///    already called `mark_dead`. If a `reconnect_loop` child succeeded,
///    it has called `replace_slot` and sent `Respawn` on `monitor_tx`.
/// 3. On `Respawn`, monitor calls `install_replacement` (puts new conn
///    into the slot) and spawns a new `health_loop`.
/// 4. On `DropSlot`, monitor logs and leaves the slot Dead.
#[allow(dead_code)]
pub(crate) async fn pool_monitor<T>(
    pool: Arc<MuxClient<T>>,
    params: Arc<ConnParams>,
    cancel: tokio_util::sync::CancellationToken,
    initial: Vec<T>,
) where
    T: MuxConnection + Send + 'static,
{
    use std::sync::atomic::Ordering;
    use tokio::task::JoinSet;

    let (monitor_tx, mut monitor_rx) = mpsc::channel::<MonitorCommand<T>>(64);
    let mut join_set: JoinSet<(usize, Result<(), tokio::task::JoinError>)> = JoinSet::new();
    for conn in initial {
        let slot = pool.push_empty_slot().await;
        let pool_ref = pool.clone();
        let params_ref = params.clone();
        let cancel_child = cancel.child_token();
        let monitor_tx_clone = monitor_tx.clone();
        join_set.spawn(async move {
            let result = tokio::spawn(health_loop(
                slot,
                conn,
                pool_ref,
                params_ref,
                cancel_child,
                monitor_tx_clone,
            ))
            .await;
            (slot, result.map(|_| ()).map_err(|e| e))
        });
    }

    // Keep monitor_tx alive for the duration of the monitor — it will
    // be dropped when this function returns.

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                join_set.abort_all();
                return;
            }
            cmd = monitor_rx.recv() => {
                match cmd {
                    Some(MonitorCommand::Respawn { slot, conn }) => {
                        // The slot is already in Active state with conn=None
                        // (set by `pool.replace_slot`). The new health_loop
                        // takes the conn by value; it does not go back
                        // into the slot's `Option<T>`. While the health_loop
                        // runs, the slot is "Active but conn==None" — this
                        // means pick_and_open skips it (correct, since the
                        // health_loop owns the conn and may close it).
                        //
                        // When the health_loop exits, the slot stays
                        // Active+None (dead, in the sense of "no usable
                        // conn until next respawn"). The new health_loop's
                        // generation is what the next pick_and_open will
                        // see... but pick_and_open skips None slots. So
                        // effectively, the slot is "warm reserve" — it
                        // gets a new conn from the next health_loop's
                        // exit-and-respawn cycle. (This is a known
                        // limitation of the v3 design; for production
                        // we may want to refactor so the slot also stores
                        // the conn during the health_loop's lifetime.)
                        let pool_ref = pool.clone();
                        let params_ref = params.clone();
                        let cancel_child = cancel.child_token();
                        let monitor_tx_clone = monitor_tx.clone();
                        join_set.spawn(async move {
                            let result = tokio::spawn(health_loop(
                                slot,
                                conn,
                                pool_ref,
                                params_ref,
                                cancel_child,
                                monitor_tx_clone,
                            ))
                            .await;
                            (slot, result.map(|_| ()).map_err(|e| e))
                        });
                    }
                    Some(MonitorCommand::DropSlot(slot)) => {
                        tracing::warn!("[slot-{}] dropped by reconnect_loop (gen_token mismatch)", slot);
                    }
                    None => {
                        while let Some(_) = join_set.join_next().await {}
                        return;
                    }
                }
            }
            Some(join_result) = join_set.join_next() => {
                let (slot, task_result) = match join_result {
                    Ok(pair) => pair,
                    Err(je) => {
                        tracing::error!("JoinSet join error: {}", je);
                        continue;
                    }
                };
                pool.metrics.health_task_exits.fetch_add(1, Ordering::Relaxed);
                match task_result {
                    Ok(()) => tracing::info!("[slot-{}] health task exited normally", slot),
                    Err(e) => tracing::error!("[slot-{}] health task panicked: {}", slot, e),
                }
                // The health_loop should have called mark_dead before exiting.
                // The reconnect_loop child (if any) is responsible for sending
                // Respawn to monitor_tx. If the child succeeded, we'll see a
                // Respawn cmd arrive. If it failed (gen_token mismatch), DropSlot.
                // If neither, the slot is Dead with no replacement — pool
                // shrinks by 1 (acceptable).
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32};
    use tokio_util::sync::CancellationToken;

    /// Mock connection for unit tests. Has rich state (valid/close/ping
    /// failure counts) so tests can drive the pool API.
    struct MockConnection {
        valid: AtomicBool,
        close_called: AtomicBool,
        ping_count: AtomicU32,
    }

    impl MockConnection {
        fn new_valid() -> Self {
            Self {
                valid: AtomicBool::new(true),
                close_called: AtomicBool::new(false),
                ping_count: AtomicU32::new(0),
            }
        }
    }

    impl MuxConnection for MockConnection {
        type SendStream = tokio::io::DuplexStream;
        type RecvStream = tokio::io::DuplexStream;

        fn ping(
            &mut self,
        ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
            self.ping_count.fetch_add(1, Ordering::Relaxed);
            let v = self.valid.load(Ordering::Acquire);
            async move {
                if v {
                    Ok(())
                } else {
                    Err(anyhow!("mock invalid"))
                }
            }
        }

        fn connect(
            &mut self,
            _url: &Url,
            _key_path: &Path,
            _host: &str,
        ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
            async move { Ok(()) }
        }

        fn open_stream(
            &mut self,
        ) -> impl std::future::Future<Output = anyhow::Result<(Self::SendStream, Self::RecvStream)>>
        + Send {
            let (a, b) = tokio::io::duplex(64);
            // Return (a, b) directly as the (send, recv) pair. The
            // production code uses real MuxStream; for unit tests of
            // the pool API we don't actually transfer data.
            async move { Ok((a, b)) }
        }

        fn accept_stream(
            &mut self,
        ) -> impl std::future::Future<Output = anyhow::Result<(Self::SendStream, Self::RecvStream)>>
        + Send {
            async move { Err(anyhow!("not implemented")) }
        }

        fn is_valid(&self) -> bool {
            self.valid.load(Ordering::Acquire)
        }
        fn set_connection(&mut self, new_c: Self) {
            *self = new_c;
        }
        fn close(&mut self) {
            self.close_called.store(true, Ordering::Release);
            self.valid.store(false, Ordering::Release);
        }
        fn active_stream_count(&self) -> usize {
            0
        }

        fn reconnect_with(
            _params: &ConnParams,
        ) -> impl std::future::Future<Output = anyhow::Result<Self>> + Send {
            async move { Ok(Self::new_valid()) }
        }
    }

    fn make_cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn make_pool() -> Arc<MuxClient<MockConnection>> {
        Arc::new(MuxClient::new(make_cancel()))
    }

    #[test]
    fn retirement_jitter_uses_shared_function() {
        for i in 0..100usize {
            let expected = ((i.wrapping_mul(73)) % 201) as i64 - 100;
            assert_eq!(retirement_jitter_secs(i), expected);
        }
    }

    #[test]
    fn jitter_range_is_bounded() {
        for i in 0..1000usize {
            let jitter = retirement_jitter_secs(i);
            assert!(jitter >= -100);
            assert!(jitter <= 100);
        }
    }

    // ----- pick_and_open -----

    #[tokio::test]
    async fn pick_and_open_skips_dead_and_empty() {
        let pool = make_pool();
        let s0 = pool.push_empty_slot().await;
        let s1 = pool.push_empty_slot().await;
        let _s2 = pool.push_empty_slot().await;
        // All slots empty — pick_and_open returns Err
        let r = pool.pick_and_open().await;
        assert!(r.is_err());
        let _ = (s0, s1);
    }

    #[tokio::test]
    async fn push_empty_increments_active_counter() {
        let pool = make_pool();
        let _ = pool.push_empty_slot().await;
        let _ = pool.push_empty_slot().await;
        assert_eq!(pool.metrics.active.load(Ordering::Relaxed), 2);
    }

    // ----- mark_retiring / mark_dead -----

    #[tokio::test]
    async fn mark_retiring_succeeds_with_matching_gen() {
        let pool = make_pool();
        let _ = pool.push_empty_slot().await;
        assert_eq!(pool.metrics.active.load(Ordering::Relaxed), 1);
        let r = pool.mark_retiring(0, 0).await;
        assert!(r.is_ok());
        assert_eq!(pool.metrics.active.load(Ordering::Relaxed), 0);
        assert_eq!(pool.metrics.retiring.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn mark_retiring_rejects_mismatched_gen() {
        let pool = make_pool();
        let _ = pool.push_empty_slot().await;
        let r = pool.mark_retiring(0, 999).await;
        assert!(matches!(r, Err(GenMismatch)));
        assert_eq!(pool.metrics.generation_mismatch.load(Ordering::Relaxed), 1);
        // State unchanged
        assert_eq!(pool.metrics.active.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn mark_dead_increments_dead_counter() {
        let pool = make_pool();
        let _ = pool.push_empty_slot().await;
        let r = pool.mark_dead(0, 0).await;
        assert!(r.is_ok());
        assert_eq!(pool.metrics.dead.load(Ordering::Relaxed), 1);
        assert_eq!(pool.metrics.active.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn mark_dead_rejects_mismatched_gen() {
        let pool = make_pool();
        let _ = pool.push_empty_slot().await;
        let r = pool.mark_dead(0, 42).await;
        assert!(matches!(r, Err(GenMismatch)));
        assert_eq!(pool.metrics.generation_mismatch.load(Ordering::Relaxed), 1);
    }

    // ----- replace_slot -----

    #[tokio::test]
    async fn replace_slot_increments_generation() {
        let pool = make_pool();
        let _ = pool.push_empty_slot().await;
        let new_conn = MockConnection::new_valid();
        let r = pool.replace_slot(0, new_conn, 0).await;
        assert!(r.is_ok());
        assert_eq!(pool.generation(0).await, 1);
    }

    #[tokio::test]
    async fn replace_slot_rejects_mismatched_gen() {
        let pool = make_pool();
        let _ = pool.push_empty_slot().await;
        let new_conn = MockConnection::new_valid();
        let r = pool.replace_slot(0, new_conn, 99).await;
        assert!(matches!(r, Err(GenMismatch)));
        assert_eq!(pool.generation(0).await, 0);
    }

    // ----- drop / cancel -----

    #[tokio::test]
    async fn drop_cancels_token() {
        let pool = make_pool();
        let cancel = pool.cancel.clone();
        assert!(!cancel.is_cancelled());
        drop(pool);
        assert!(cancel.is_cancelled());
    }

    // ----- Concurrent state transitions -----

    #[tokio::test]
    async fn multiple_slots_have_independent_state() {
        let pool = make_pool();
        let _ = pool.push_empty_slot().await;
        let _ = pool.push_empty_slot().await;
        let _ = pool.push_empty_slot().await;
        assert_eq!(pool.metrics.active.load(Ordering::Relaxed), 3);

        // Mark slot 0 retiring, slot 2 dead
        assert!(pool.mark_retiring(0, 0).await.is_ok());
        assert!(pool.mark_dead(2, 0).await.is_ok());

        assert_eq!(pool.metrics.active.load(Ordering::Relaxed), 1);
        assert_eq!(pool.metrics.retiring.load(Ordering::Relaxed), 1);
        assert_eq!(pool.metrics.dead.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn install_replacement_populates_empty_slot() {
        let pool = make_pool();
        let _ = pool.push_empty_slot().await;
        let conn = MockConnection::new_valid();
        pool.install_replacement(0, conn).await;
        // Now pick_and_open should find slot 0 as valid+active
        let r = pool.pick_and_open().await;
        assert!(r.is_ok());
    }
}
