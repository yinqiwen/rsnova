use anyhow::anyhow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use url::Url;

use crate::mux::event;
use crate::mux::event::{OpenStreamEvent, StreamProto};

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
                proto: StreamProto::Tcp,
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
                proto: StreamProto::Udp,
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
    fn ping(&mut self) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn connect(
        &mut self,
        url: &Url,
        key_path: &Path,
        host: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn open_stream(
        &mut self,
    ) -> impl std::future::Future<Output = anyhow::Result<(Self::SendStream, Self::RecvStream)>> + Send;
    fn accept_stream(
        &mut self,
    ) -> impl std::future::Future<Output = anyhow::Result<(Self::SendStream, Self::RecvStream)>> + Send;
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

pub(crate) fn validate_pool_config(
    count: usize,
    ping_interval_secs: u64,
    ping_fail_threshold: u32,
) -> anyhow::Result<()> {
    if count == 0 {
        return Err(anyhow!("connection pool size must be >= 1"));
    }
    if ping_interval_secs == 0 {
        return Err(anyhow!("--ping-interval must be >= 1"));
    }
    if ping_fail_threshold == 0 {
        return Err(anyhow!("--ping-fail-threshold must be >= 1"));
    }
    Ok(())
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
#[derive(Debug)]
pub(crate) struct GenMismatch;

/// State of a pool slot. Transitions: Active → Retiring → Dead → (replace) → Active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotState {
    Active,
    Retiring,
    Dead,
}

/// One entry in the connection pool.
pub(crate) struct SlotEntry<T> {
    /// Shared reference to the connection. Both the pool (via `pick_and_open`)
    /// and the `health_loop` access the conn through this Arc. `None` only
    /// during the brief window between `replace_slot` and reconnect_loop
    /// writing the new conn.
    pub(crate) conn: Arc<tokio::sync::Mutex<Option<T>>>,
    pub(crate) state: SlotState,
    pub(crate) generation: Generation,
}

/// Static parameters for connection creation. Owned by the pool monitor
/// and shared (via `Arc<ConnParams>`) with each health task.
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
pub(crate) enum MonitorCommand {
    /// `reconnect_loop` succeeded and wrote the new conn into the slot's
    /// `conn_ref`. Monitor spawns a new health task for this slot.
    Respawn { slot: usize },
    /// Reconnect abandoned the slot (e.g. gen_token mismatch). Monitor logs
    /// and accepts the slot as permanently dead (pool shrinks by 1).
    DropSlot(usize),
}

pub(crate) struct MuxClient<T> {
    pub(crate) conns: tokio::sync::Mutex<Vec<SlotEntry<T>>>,
    pub(crate) cursor: AtomicUsize,
    pub(crate) cancel: tokio_util::sync::CancellationToken,
    pub(crate) metrics: Arc<PoolMetrics>,
}

impl<T: MuxConnection> MuxClient<T> {
    /// Create an empty pool. Initial connections are pushed in via
    /// `push_initial_slot` by the `pool_monitor`.
    pub(crate) fn new(cancel: tokio_util::sync::CancellationToken) -> Self {
        Self {
            conns: tokio::sync::Mutex::new(Vec::new()),
            cursor: AtomicUsize::new(0),
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

    /// Push an empty slot and return the slot index plus a clone of the
    /// shared `conn_ref`. The caller (pool_monitor) writes the initial conn
    /// into `conn_ref` and passes clones to health_loop / reconnect_loop.
    pub(crate) async fn push_empty_slot(&self) -> (usize, Arc<tokio::sync::Mutex<Option<T>>>) {
        let conn_ref = Arc::new(tokio::sync::Mutex::new(None));
        let mut guard = self.conns.lock().await;
        let slot = guard.len();
        guard.push(SlotEntry {
            conn: conn_ref.clone(),
            state: SlotState::Active,
            generation: 0,
        });
        self.metrics.active.fetch_add(1, Ordering::Relaxed);
        (slot, conn_ref)
    }

    /// Pick an Active connection and call `open_stream` on it.
    ///
    /// The pool lock is held only once per call — long enough to snapshot the
    /// list of `(conn_ref, state)` pairs. The snapshot is then iterated without
    /// further mutex contention. Busy conn_ref locks are skipped with
    /// `try_lock`; if all usable candidates are busy, we await one busy
    /// candidate as a fallback.
    pub(crate) async fn pick_and_open(&self) -> anyhow::Result<(T::SendStream, T::RecvStream)> {
        // Single lock to snapshot the slot list. Each `conn_ref` is an
        // `Arc<Mutex<Option<T>>>`, so cloning it is cheap (refcount bump).
        let snapshot: Vec<(Arc<tokio::sync::Mutex<Option<T>>>, SlotState)> = {
            let guard = self.conns.lock().await;
            guard
                .iter()
                .map(|e| (e.conn.clone(), e.state))
                .collect::<Vec<_>>()
        };
        let len = snapshot.len();
        if len == 0 {
            tracing::error!("no available stream: pool is empty");
            return Err(anyhow!("no available stream"));
        }

        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        let mut fallback_busy_conn = None;

        for offset in 0..len {
            let (conn_ref, state) = &snapshot[(start + offset) % len];
            if *state != SlotState::Active {
                continue;
            }

            if let Ok(mut conn_guard) = conn_ref.try_lock() {
                let Some(conn) = conn_guard.as_mut() else {
                    continue;
                };
                if !conn.is_valid() {
                    continue;
                }
                return conn.open_stream().await;
            }

            if fallback_busy_conn.is_none() {
                fallback_busy_conn = Some(conn_ref.clone());
            }
        }

        if let Some(conn_ref) = fallback_busy_conn {
            let mut conn_guard = conn_ref.lock().await;
            if let Some(conn) = conn_guard.as_mut()
                && conn.is_valid()
            {
                return conn.open_stream().await;
            }
        }

        let active = self.metrics.active.load(Ordering::Relaxed);
        let retiring = self.metrics.retiring.load(Ordering::Relaxed);
        let dead = self.metrics.dead.load(Ordering::Relaxed);
        tracing::error!(
            "no available stream: pool size={}, active={}, retiring={}, dead={}",
            len,
            active,
            retiring,
            dead,
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
            {
                let mut conn_guard = entry.conn.lock().await;
                if let Some(mut conn) = conn_guard.take() {
                    conn.close();
                }
            }
            entry.state = SlotState::Dead;
            self.metrics.dead.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Replace the slot's state, increment generation, mark Active.
    /// The caller (`reconnect_loop`) writes the new conn into the slot's
    /// `conn_ref` directly after this call succeeds. This method does NOT
    /// touch the conn — it only updates state and generation.
    pub(crate) async fn replace_slot(
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
            tracing::warn!(
                "[slot-{}] replace_slot rejected: expected Dead, got {:?}",
                slot,
                entry.state
            );
            return Err(GenMismatch);
        }
        entry.generation = entry.generation.wrapping_add(1);
        entry.state = SlotState::Active;
        self.metrics.dead.fetch_sub(1, Ordering::Relaxed);
        self.metrics.active.fetch_add(1, Ordering::Relaxed);
        Ok(())
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
                match tokio::time::timeout(Duration::from_secs(1), client.pick_and_open()).await {
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
                                if let Some(payload) = event.payload
                                    && let Err(e) = send.write_all(&payload).await
                                {
                                    tracing::error!("write payload failed:{}", e);
                                    metrics::gauge!("client_proxy_streams").decrement(1.0);
                                    return;
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
                                let (mut local_reader, mut local_writer) =
                                    tokio::io::split(udp_stream);
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
                                if let Some(payload) = event.payload
                                    && let Err(e) = send.write_all(&payload).await
                                {
                                    tracing::error!("write payload failed:{}", e);
                                    metrics::gauge!("client_proxy_streams").decrement(1.0);
                                    return;
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

/// Per-connection health loop. Each connection has one. Pings the conn
/// (via `conn_ref`), transitions to Retiring on N consecutive ping failures
/// or max_age reached, spawns a `reconnect_loop` child task that produces a
/// fresh conn. Exits only when:
/// - `cancel` fires (graceful shutdown)
/// - the original conn dies AND the reconnect child has finished
///
/// The child reconnect_loop is responsible for sending to `monitor_tx`
/// on success. The parent `health_loop` only waits for the child to
/// complete before exiting.
pub(crate) async fn health_loop<T>(
    slot: usize,
    conn_ref: Arc<tokio::sync::Mutex<Option<T>>>,
    pool: Arc<MuxClient<T>>,
    params: Arc<ConnParams>,
    cancel: tokio_util::sync::CancellationToken,
    monitor_tx: mpsc::Sender<MonitorCommand>,
) where
    T: MuxConnection + Send + 'static,
{
    let gen_token = pool.generation(slot).await;
    let mut is_retiring = false;
    let mut consecutive_fails: u32 = 0;
    let max_age_deadline = params.max_age.map(|d| tokio::time::Instant::now() + d);

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
                // Phase 1: ping the conn via conn_ref
                // We must drop the conn_ref guard before calling mark_retiring /
                // mark_dead to avoid lock ordering issues (pick_and_open takes
                // conns lock first, then conn_ref lock; we must not hold
                // conn_ref while acquiring conns lock).
                {
                    let mut should_mark_retiring = false;
                    {
                        let mut guard = conn_ref.lock().await;
                        if let Some(conn) = guard.as_mut() {
                            if conn.is_valid() {
                                if conn.ping().await.is_err() {
                                    consecutive_fails += 1;
                                    if consecutive_fails >= params.ping_fail_threshold {
                                        should_mark_retiring = true;
                                    }
                                } else {
                                    consecutive_fails = 0;
                                }
                            } else {
                                should_mark_retiring = true;
                            }
                        } else {
                            // conn taken by reconnect_loop or mark_dead
                            should_mark_retiring = true;
                        }
                    }
                    // guard is dropped; safe to acquire conns lock
                    if should_mark_retiring && !is_retiring {
                        is_retiring = true;
                        if pool.mark_retiring(slot, gen_token).await.is_err() {
                            return;
                        }
                    }
                }

                // max_age check. The current slot model cannot keep both a draining
                // old connection and a fresh replacement in the same conn_ref. Close
                // the old connection and move to Dead before reconnecting.
                if !is_retiring
                    && let Some(deadline) = max_age_deadline
                    && tokio::time::Instant::now() >= deadline
                {
                    tracing::info!("[slot-{}] reached max age, reconnecting", slot);
                    is_retiring = true;
                    if pool.mark_dead(slot, gen_token).await.is_err() {
                        return;
                    }
                }

                // Spawn reconnect child if needed. The child sends to
                // monitor_tx directly on success or DropSlot.
                if is_retiring && reconnect_handle.is_none() {
                    let pool_ref = pool.clone();
                    let params_ref = params.clone();
                    let cancel_child = cancel.child_token();
                    let monitor_tx_clone = monitor_tx.clone();
                    let conn_ref_clone = conn_ref.clone();
                    reconnect_handle = Some(tokio::spawn(async move {
                        let cmd = reconnect_loop(
                            slot,
                            conn_ref_clone,
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
                if is_retiring {
                    let conn_dead = {
                        let guard = conn_ref.lock().await;
                        guard.as_ref().is_none_or(|c| !c.is_valid())
                    };
                    if conn_dead {
                        if pool.mark_dead(slot, gen_token).await.is_err() {
                            return;
                        }
                        if let Some(h) = reconnect_handle.take() {
                            let _ = h.await;
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// Indefinitely retries `T::reconnect_with` with exponential backoff and
/// ±20% jitter. On success, calls `pool.replace_slot` (updates state +
/// generation) then writes the new conn directly into `conn_ref`. On
/// gen_token-mismatch or other unrecoverable error, sends
/// `MonitorCommand::DropSlot` and returns.
async fn reconnect_loop<T>(
    slot: usize,
    conn_ref: Arc<tokio::sync::Mutex<Option<T>>>,
    pool: Arc<MuxClient<T>>,
    params: Arc<ConnParams>,
    gen_token: Generation,
    cancel: tokio_util::sync::CancellationToken,
) -> MonitorCommand
where
    T: MuxConnection + Send + 'static,
{
    use std::sync::atomic::Ordering;
    let _permit = reconnect_limiter()
        .acquire()
        .await
        .expect("static semaphore never closes");
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
        pool.metrics
            .reconnect_attempts
            .fetch_add(1, Ordering::Relaxed);

        match T::reconnect_with(&params).await {
            Ok(new_conn) => match pool.replace_slot(slot, gen_token).await {
                Ok(()) => {
                    // Write the new conn directly into the shared conn_ref.
                    {
                        let mut guard = conn_ref.lock().await;
                        *guard = Some(new_conn);
                    }
                    pool.metrics
                        .reconnect_success
                        .fetch_add(1, Ordering::Relaxed);
                    return MonitorCommand::Respawn { slot };
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
                tracing::warn!(
                    "[slot-{}] reconnect failed: {}; backoff {:?}",
                    slot,
                    e,
                    backoff
                );
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Pool-level supervisor. Owns the lifecycle of all `health_loop`s.
///
/// Architecture:
/// 1. `pool_monitor` calls `push_empty_slot` for each initial conn, writes
///    the conn into the slot's `conn_ref`, then spawns `health_loop` with a
///    clone of the `conn_ref`. The slot is Active with a populated conn_ref.
/// 2. `health_loop` pings via `conn_ref`, detects death, spawns `reconnect_loop`.
/// 3. `reconnect_loop` on success: calls `replace_slot` (updates state/gen),
///    writes new conn directly into `conn_ref`, sends `Respawn` to monitor.
/// 4. On `Respawn`, monitor clones `conn_ref` from slot entry and spawns a
///    new `health_loop`.
/// 5. On `DropSlot`, monitor logs and leaves the slot Dead.
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

    let (monitor_tx, mut monitor_rx) = mpsc::channel::<MonitorCommand>(64);
    let mut join_set: JoinSet<(usize, Result<(), tokio::task::JoinError>)> = JoinSet::new();
    for conn in initial {
        let (slot, conn_ref) = pool.push_empty_slot().await;
        // Write the initial conn into the shared conn_ref.
        {
            let mut guard = conn_ref.lock().await;
            *guard = Some(conn);
        }
        let pool_ref = pool.clone();
        let params_ref = params.clone();
        let cancel_child = cancel.child_token();
        let monitor_tx_clone = monitor_tx.clone();
        join_set.spawn(async move {
            let result = tokio::spawn(health_loop(
                slot,
                conn_ref,
                pool_ref,
                params_ref,
                cancel_child,
                monitor_tx_clone,
            ))
            .await;
            (slot, result.map(|_| ()))
        });
    }

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                join_set.abort_all();
                return;
            }
            cmd = monitor_rx.recv() => {
                match cmd {
                    Some(MonitorCommand::Respawn { slot }) => {
                        // Clone the conn_ref from the slot entry for the new health_loop.
                        let conn_ref = {
                            let guard = pool.conns.lock().await;
                            guard[slot].conn.clone()
                        };
                        let pool_ref = pool.clone();
                        let params_ref = params.clone();
                        let cancel_child = cancel.child_token();
                        let monitor_tx_clone = monitor_tx.clone();
                        join_set.spawn(async move {
                            let result = tokio::spawn(health_loop(
                                slot,
                                conn_ref,
                                pool_ref,
                                params_ref,
                                cancel_child,
                                monitor_tx_clone,
                            ))
                            .await;
                            (slot, result.map(|_| ()))
                        });
                    }
                    Some(MonitorCommand::DropSlot(slot)) => {
                        tracing::warn!("[slot-{}] dropped by reconnect_loop (gen_token mismatch)", slot);
                    }
                    None => {
                        // All monitor_tx senders dropped; drain remaining tasks.
                        while join_set.join_next().await.is_some() {}
                        return;
                    }
                }
            }
            // join_next() returns None when the JoinSet is empty, which means
            // select! simply doesn't trigger this branch — correct behavior.
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
        open_count: Arc<AtomicU32>,
        open_started: Option<Arc<tokio::sync::Notify>>,
        open_gate: Option<Arc<tokio::sync::Notify>>,
    }

    impl MockConnection {
        fn new_valid() -> Self {
            Self::new_with_open_count(Arc::new(AtomicU32::new(0)))
        }

        fn new_with_open_count(open_count: Arc<AtomicU32>) -> Self {
            Self {
                valid: AtomicBool::new(true),
                close_called: AtomicBool::new(false),
                ping_count: AtomicU32::new(0),
                open_count,
                open_started: None,
                open_gate: None,
            }
        }

        fn new_with_blocked_open(
            open_started: Arc<tokio::sync::Notify>,
            open_gate: Arc<tokio::sync::Notify>,
        ) -> Self {
            Self {
                valid: AtomicBool::new(true),
                close_called: AtomicBool::new(false),
                ping_count: AtomicU32::new(0),
                open_count: Arc::new(AtomicU32::new(0)),
                open_started: Some(open_started),
                open_gate: Some(open_gate),
            }
        }
    }

    impl MuxConnection for MockConnection {
        type SendStream = tokio::io::DuplexStream;
        type RecvStream = tokio::io::DuplexStream;

        fn ping(&mut self) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
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
        ) -> impl std::future::Future<Output = anyhow::Result<(Self::SendStream, Self::RecvStream)>> + Send
        {
            self.open_count.fetch_add(1, Ordering::Relaxed);
            let open_started = self.open_started.clone();
            let open_gate = self.open_gate.clone();
            let (a, b) = tokio::io::duplex(64);
            // Return (a, b) directly as the (send, recv) pair. The
            // production code uses real MuxStream; for unit tests of
            // the pool API we don't actually transfer data.
            async move {
                if let Some(started) = open_started {
                    started.notify_one();
                }
                if let Some(gate) = open_gate {
                    gate.notified().await;
                }
                Ok((a, b))
            }
        }

        fn accept_stream(
            &mut self,
        ) -> impl std::future::Future<Output = anyhow::Result<(Self::SendStream, Self::RecvStream)>> + Send
        {
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

    #[test]
    fn validate_pool_config_rejects_zero_values() {
        assert!(validate_pool_config(0, 1, 1).is_err());
        assert!(validate_pool_config(1, 0, 1).is_err());
        assert!(validate_pool_config(1, 1, 0).is_err());
        assert!(validate_pool_config(1, 1, 1).is_ok());
    }

    // ----- pick_and_open -----

    #[tokio::test]
    async fn pick_and_open_skips_dead_and_empty() {
        let pool = make_pool();
        let (s0, _) = pool.push_empty_slot().await;
        let (s1, _) = pool.push_empty_slot().await;
        let (_s2, _) = pool.push_empty_slot().await;
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

    #[tokio::test]
    async fn pick_and_open_distributes_across_active_slots_round_robin() {
        let pool = make_pool();
        let mut counts = Vec::new();
        for _ in 0..3 {
            let (_, conn_ref) = pool.push_empty_slot().await;
            let count = Arc::new(AtomicU32::new(0));
            {
                let mut guard = conn_ref.lock().await;
                *guard = Some(MockConnection::new_with_open_count(count.clone()));
            }
            counts.push(count);
        }

        for _ in 0..6 {
            assert!(pool.pick_and_open().await.is_ok());
        }

        let opened: Vec<u32> = counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        assert_eq!(opened, vec![2, 2, 2]);
    }

    #[tokio::test]
    async fn pick_and_open_does_not_hold_pool_lock_while_open_stream_is_pending() {
        let pool = make_pool();
        let (_, conn_ref) = pool.push_empty_slot().await;
        let open_started = Arc::new(tokio::sync::Notify::new());
        let open_gate = Arc::new(tokio::sync::Notify::new());
        {
            let mut guard = conn_ref.lock().await;
            *guard = Some(MockConnection::new_with_blocked_open(
                open_started.clone(),
                open_gate.clone(),
            ));
        }

        let pool_for_open = pool.clone();
        let open_task = tokio::spawn(async move { pool_for_open.pick_and_open().await });
        open_started.notified().await;

        let generation = tokio::time::timeout(Duration::from_millis(100), pool.generation(0)).await;
        assert!(
            generation.is_ok(),
            "pool lock should not be held by open_stream"
        );

        open_gate.notify_one();
        assert!(open_task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn pick_and_open_skips_busy_connection_when_another_active_slot_is_free() {
        let pool = make_pool();
        let (_, busy_conn_ref) = pool.push_empty_slot().await;
        let (_, free_conn_ref) = pool.push_empty_slot().await;

        let busy_started = Arc::new(tokio::sync::Notify::new());
        let busy_gate = Arc::new(tokio::sync::Notify::new());
        {
            let mut guard = busy_conn_ref.lock().await;
            *guard = Some(MockConnection::new_with_blocked_open(
                busy_started.clone(),
                busy_gate.clone(),
            ));
        }

        let free_open_count = Arc::new(AtomicU32::new(0));
        {
            let mut guard = free_conn_ref.lock().await;
            *guard = Some(MockConnection::new_with_open_count(free_open_count.clone()));
        }

        let pool_for_busy = pool.clone();
        let busy_task = tokio::spawn(async move { pool_for_busy.pick_and_open().await });
        busy_started.notified().await;

        // Force the next pick to consider the locked slot first. A busy-aware
        // implementation should try_lock, skip it, and use the second slot.
        pool.cursor.store(0, Ordering::Relaxed);
        let skipped_busy = tokio::time::timeout(Duration::from_millis(100), pool.pick_and_open())
            .await
            .expect("pick_and_open should skip a locked busy connection");

        assert!(skipped_busy.is_ok());
        assert_eq!(free_open_count.load(Ordering::Relaxed), 1);

        busy_gate.notify_one();
        assert!(busy_task.await.unwrap().is_ok());
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
        // Must be Dead state for replace_slot to succeed
        pool.mark_dead(0, 0).await.unwrap();
        let r = pool.replace_slot(0, 0).await;
        assert!(r.is_ok());
        assert_eq!(pool.generation(0).await, 1);
    }

    #[tokio::test]
    async fn replace_slot_rejects_mismatched_gen() {
        let pool = make_pool();
        let _ = pool.push_empty_slot().await;
        pool.mark_dead(0, 0).await.unwrap();
        let r = pool.replace_slot(0, 99).await;
        assert!(matches!(r, Err(GenMismatch)));
        assert_eq!(pool.generation(0).await, 0);
    }

    #[tokio::test]
    async fn replace_slot_rejects_retiring_slot_without_dead_counter_underflow() {
        let pool = make_pool();
        let _ = pool.push_empty_slot().await;
        pool.mark_retiring(0, 0).await.unwrap();

        let r = pool.replace_slot(0, 0).await;

        assert!(r.is_err(), "replace_slot must only replace Dead slots");
        assert_eq!(pool.generation(0).await, 0);
        assert_eq!(pool.slot_state(0).await, SlotState::Retiring);
        assert_eq!(pool.metrics.dead.load(Ordering::Relaxed), 0);
        assert_eq!(pool.metrics.active.load(Ordering::Relaxed), 0);
        assert_eq!(pool.metrics.retiring.load(Ordering::Relaxed), 1);
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

    // ----- Shared conn_ref -----

    #[tokio::test]
    async fn conn_ref_shared_access() {
        let pool = make_pool();
        let (slot, conn_ref) = pool.push_empty_slot().await;
        // Write a conn into the shared ref
        {
            let mut guard = conn_ref.lock().await;
            *guard = Some(MockConnection::new_valid());
        }
        // Verify pool sees it via pick_and_open
        let r = pool.pick_and_open().await;
        assert!(r.is_ok());
        let _ = slot;
    }

    #[tokio::test]
    async fn pick_and_open_via_conn_ref() {
        let pool = make_pool();
        let (slot, conn_ref) = pool.push_empty_slot().await;
        // Slot is Active but conn_ref is None — pick_and_open should skip
        assert!(pool.pick_and_open().await.is_err());
        // Write a valid conn
        {
            let mut guard = conn_ref.lock().await;
            *guard = Some(MockConnection::new_valid());
        }
        // Now pick_and_open should succeed
        assert!(pool.pick_and_open().await.is_ok());
        let _ = slot;
    }

    #[tokio::test]
    async fn mark_dead_takes_conn_from_ref() {
        let pool = make_pool();
        let (slot, conn_ref) = pool.push_empty_slot().await;
        let conn = MockConnection::new_valid();
        {
            let mut guard = conn_ref.lock().await;
            *guard = Some(conn);
        }
        // mark_dead should take the conn out and close it
        pool.mark_dead(slot, 0).await.unwrap();
        let guard = conn_ref.lock().await;
        assert!(guard.is_none(), "conn_ref should be None after mark_dead");
    }

    #[tokio::test]
    async fn reconnect_writes_to_conn_ref() {
        let pool = make_pool();
        let (slot, conn_ref) = pool.push_empty_slot().await;
        // Simulate: mark dead, replace_slot, write new conn
        pool.mark_dead(slot, 0).await.unwrap();
        pool.replace_slot(slot, 0).await.unwrap();
        {
            let mut guard = conn_ref.lock().await;
            *guard = Some(MockConnection::new_valid());
        }
        // Verify the new conn is usable
        assert!(pool.pick_and_open().await.is_ok());
    }
}
