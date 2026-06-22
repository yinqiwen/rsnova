# Connection Self-Managed Health Design (Final)

## 核心思想

把"健康检查 + 自动重连"的职责从 `MuxClient`（池）下放到每个 `MuxConnection` 实例自身。每个连接是一个自包含的活跃实体。

- **连接状态机**：`Active` → `Retiring` → `Dead`（终止）
- **后台健康任务**：每个连接 spawn 一个 task，负责 ping 健康检查、检测死亡、自动重连
- **池极简化**：只负责"挑选可用的连接" + "接收替换"
- **统一监督者**：`pool_monitor` 是 health task handle 的唯一管理者

## 关键不变量

1. **每个槽位有且仅有一个 health task 在运行**（由 monitor 统一管理）
2. **重连是连接的内部循环**——一旦进入重连，永不放弃（带 per-slot backoff）
3. **池永不丢失槽位**——health task 永不"正常退出"；panic 由 monitor 兜底
4. **重连成功后立即交接**——`replace_slot` 内部只更新池状态；新 health task 由 monitor 启动
5. **过期 task 不会污染池**——generation token 校验

## 状态机

```
       spawn_health_task (by monitor)
         │
         ▼
   ┌─ Active ─────────────────────────────┐
   │ (ping OK, serving new streams)        │
   │                                       │
   │ (max_age reached)                     │ (N consecutive ping fails)
   ▼                                       ▼
Retiring (mark_retiring + 启动重连子任务)
   │                                       │
   │ (原 task 继续 ping 检测原连接死亡)    │ (原 task mark_dead + close)
   │                                       │
   ▼                                       ▼
Dead (停止 ping, 等待重连子任务完成)
   │
   ▼
   (重连子任务成功 → pool.replace_slot)
   │
   ▼
   旧 task 退出 (新 task 由 monitor 启动)
```

**关键修正**（vs v1）:
- Retiring 立即启动重连子任务（不等老流结束）
- 原 task 在 Retiring 阶段继续 ping，检测原连接意外死亡时快速 mark_dead + close
- 重连是**子任务**，原 task 监督它

## 池 API（最终版）

```rust
type Generation = u64;

#[derive(Clone, Copy, PartialEq)]
enum SlotState {
    Active,
    Retiring,
    Dead,
}

struct SlotEntry<T> {
    conn: T,
    state: SlotState,
    generation: Generation,
}

impl<T: MuxConnection> MuxClient<T> {
    /// Atomic: pick an Active+valid connection and call open_stream on it.
    /// Returns the streams or an error. Holds lock for entire operation
    /// to eliminate TOCTOU races.
    async fn pick_and_open(&self) -> anyhow::Result<(T::SendStream, T::RecvStream)>;

    /// Health task: max_age reached. Sets state to Retiring.
    async fn mark_retiring(&self, slot: usize, gen: Generation) -> Result<(), GenMismatch>;

    /// Health task: N consecutive ping fails. Closes conn, sets state to Dead.
    async fn mark_dead(&self, slot: usize, gen: Generation) -> Result<(), GenMismatch>;

    /// Health task / monitor: after successful reconnect.
    /// Replaces the slot, increments generation, sets state to Active.
    /// Does NOT spawn a new health task — monitor will do that.
    async fn replace_slot(&self, slot: usize, new_conn: T, gen: Generation)
        -> Result<Generation, GenMismatch>;

    /// Returns current state of a slot (for health task decision making).
    async fn slot_state(&self, slot: usize) -> SlotState;
}
```

**关键修正**（vs v2）:
- `pick_and_open` 是原子的，单次持锁完成选择和 open_stream
- `replace_slot` **不**自己 spawn 新 health task
- 状态用 `enum SlotState` 替代 bool `retired` + `is_valid()` 的隐式组合

## Health Task 结构（最终版）

```rust
async fn health_loop<T: MuxConnection>(
    slot: usize,
    conn: T,
    pool: Arc<MuxClient<T>>,
    params: Arc<ConnParams>,
    cancel: CancellationToken,
) {
    let gen = pool.generation(slot);
    let mut is_retiring = false;
    let mut consecutive_fails = 0u32;
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(60);
    let mut reconnect_handle: Option<JoinHandle<()>> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,

            // ping tick
            _ = sleep(params.ping_interval) => {
                // Phase 1: ping 原连接
                if conn.is_valid() {
                    match conn.ping().await {
                        Ok(()) => consecutive_fails = 0,
                        Err(_) => {
                            consecutive_fails += 1;
                            if consecutive_fails >= params.ping_fail_threshold && !is_retiring {
                                is_retiring = true;
                                let _ = pool.mark_retiring(slot, gen).await;
                            }
                        }
                    }
                } else if !is_retiring {
                    // is_valid=false → close 已经发生（由别人 mark_dead 触发，
                    // 或远端死亡导致 mux dispatcher 退出）
                    is_retiring = true;
                    let _ = pool.mark_retiring(slot, gen).await;
                }

                // max_age 检查
                if !is_retiring && is_max_age_reached(...) {
                    is_retiring = true;
                    let _ = pool.mark_retiring(slot, gen).await;
                }

                // 如果进入 Retiring 但还没有重连子任务，启动一个
                if is_retiring && reconnect_handle.is_none() {
                    let pool_ref = pool.clone();
                    let params_ref = params.clone();
                    let conn_clone = conn.clone_handle();  // 用于重连的连接描述符
                    reconnect_handle = Some(tokio::spawn(async move {
                        reconnect_loop(slot, conn_clone, pool_ref, params_ref, backoff).await;
                    }));
                }

                // 如果 is_retiring 且 is_valid=false：明确 mark_dead + close
                if is_retiring && !conn.is_valid() {
                    conn.close();
                    let _ = pool.mark_dead(slot, gen).await;
                    // 等待 reconnect 子任务
                    if let Some(h) = reconnect_handle.take() {
                        let _ = h.await;
                    }
                    return;  // 重连子任务已调 replace_slot，新 task 由 monitor 启动
                }
            }
        }
    }
}

async fn reconnect_loop<T>(
    slot: usize,
    conn_template: T,  // 持有以保持类型/参数；实际重连会创建新 T
    pool: Arc<MuxClient<T>>,
    params: Arc<ConnParams>,
    initial_backoff: Duration,
) where T: MuxConnection + Clone  // 通过 Clone 传递参数模板
{
    let mut backoff = initial_backoff;
    let _permit = reconnect_limiter.acquire().await;

    loop {
        sleep_with_jitter(backoff).await;
        let gen = pool.generation(slot);

        match try_reconnect(&params).await {
            Ok(new_conn) => {
                match pool.replace_slot(slot, new_conn, gen).await {
                    Ok(_) => return,  // 成功；monitor 会看到 handle 退出
                    Err(GenMismatch) => return,  // 池状态已被别人接管
                }
            }
            Err(_) => {
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}
```

**关键修正**（vs v1）:
- 明确两阶段：原 task 做 ping + 状态转换；重连是独立子任务
- 原 task 在 Retiring 阶段继续 ping，检测原连接死亡
- 原 task 在重连期间监督重连子任务，完成后退出

## ConnParams

```rust
struct ConnParams {
    url: Url,
    cert_path: PathBuf,
    host: String,
    stream_window: u32,
    max_age: Option<Duration>,         // None = 永不过期
    ping_interval: Duration,           // default 5s
    ping_fail_threshold: u32,          // default 3
}
```

## 通信方式

**删除 `Message::ConnectionReady` 通道路径**。`health_task` 和 monitor **直接调用** `pool.replace_slot`，用 `Arc<Mutex<MuxClient>>` 保护。

新的 Message enum 极简化：

```rust
pub enum Message {
    OpenStream(OpenStreamRequest),
}
```

## Pool Monitor（统一 handle 管理）

```rust
async fn pool_monitor<T>(
    pool: Arc<MuxClient<T>>,
    params: Arc<ConnParams>,
    cancel: CancellationToken,
    initial_conns: Vec<T>,  // 启动时的初始连接
) {
    let mut join_set: JoinSet<(usize, Result<(), tokio::task::JoinError>)> = JoinSet::new();

    // 启动所有初始 health task
    for (slot, conn) in initial_conns.into_iter().enumerate() {
        let pool_ref = pool.clone();
        let params_ref = params.clone();
        let cancel_child = cancel.child_token();
        join_set.spawn(async move {
            let result = tokio::spawn(health_loop(slot, conn, pool_ref, params_ref, cancel_child))
                .await;
            (slot, result)
        });
    }

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                join_set.abort_all();
                return;
            }
            Some(join_result) = join_set.join_next() => {
                let (slot, task_result) = match join_result {
                    Ok(pair) => pair,
                    Err(join_error) => {
                        tracing::error!("JoinSet join error: {}", join_error);
                        continue;
                    }
                };
                metrics::counter!("mux.pool.health_task_exits").increment(1);

                // 检查槽位状态决定下一步
                let state = pool.slot_state(slot).await;
                match state {
                    SlotState::Active => {
                        // 异常退出（不应该发生）；强制 mark_dead
                        tracing::error!("Active slot {} health task exited unexpectedly", slot);
                        let _ = pool.mark_dead(slot, /* current gen */).await;
                    }
                    SlotState::Retiring | SlotState::Dead => {
                        // 预期路径：health task 在原连接死亡后退出；
                        // 重连子任务应该已经调了 replace_slot
                        // 现在只需要 spawn 新的 health task
                    }
                }

                // 检查槽位是否真的被替换
                if pool.slot_state(slot).await == SlotState::Active {
                    // 已被替换，spawn 新 health task
                    let new_conn = pool.take_new_conn(slot).await;
                    if let Some(new_conn) = new_conn {
                        let pool_ref = pool.clone();
                        let params_ref = params.clone();
                        let cancel_child = cancel.child_token();
                        join_set.spawn(async move {
                            let result = tokio::spawn(health_loop(slot, new_conn, pool_ref, params_ref, cancel_child))
                                .await;
                            (slot, result)
                        });
                    }
                } else {
                    // 槽位仍为 Dead/Retiring，说明重连没完成；
                    // 重新 spawn health task 让它继续监督（无限重试）
                    // 注意：此时槽位没有 conn，需要占位
                    tracing::warn!("Slot {} still dead after health task exit, respawning health task with placeholder", slot);
                    // ... 启动一个"等待重连"任务
                }
            }
        }
    }
}
```

**关键修正**（vs v2）:
- 用 `JoinSet` 替代 `HashMap<JoinHandle>`（删除冗余）
- monitor 是 handle 管理的**唯一**入口
- `replace_slot` 不再自己 spawn task，由 monitor 统一处理

## 优雅退出

```rust
pub struct MuxClient<T> {
    conns: Mutex<Vec<SlotEntry<T>>>,
    cancel: CancellationToken,
    metrics: Arc<PoolMetrics>,
}

impl<T> Drop for MuxClient<T> {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
```

所有 health task 和 monitor 持 `cancel.child_token()`，父级 `cancel` 取消时级联取消。

## Metrics

```rust
struct PoolMetrics {
    active: AtomicUsize,
    retiring: AtomicUsize,
    dead: AtomicUsize,
    reconnect_attempts: AtomicU64,
    reconnect_success: AtomicU64,
    health_task_panics: AtomicU64,
    generation_mismatch: AtomicU64,
}

// 这些 AtomicUsize 暴露为 metrics::gauge! 在 health_loop/monitor 中更新
```

## 并发安全分析（完整版）

| 场景 | 安全性 |
|------|--------|
| 两个 health task 同时 mark_dead | generation 校验，第二个看到 mismatch 退出 |
| pick_and_open 与 mark_retiring | 都在锁内，串行化 |
| pick_and_open 与 replace_slot | 同上 |
| health task 在 mark_dead 后 panic | monitor 看到 handle 退出，重新 spawn（槽位已 Dead，monitor 启动"等待重连"任务） |
| health task 在 replace_slot 后 panic | 新连接已在池中；monitor 启动新 health task |
| 重连期间连接被 pick | is_valid=false，被 pick_and_open 跳过 |
| MuxClient::drop | cancel.cancel() 级联取消所有 health task |
| Generation mismatch | 被拒绝；旧 task 静默退出 |
| 所有连接同时死亡 | Semaphore(2) 限制最多 2 个并发重连 |
| Retiring 期间原连接意外死亡 | 原 task 继续 ping，检测到死亡后 close + mark_dead |

## 实施步骤

1. **第一步：定义新类型** — `ConnParams`, `SlotState`, `SlotEntry`, `PoolMetrics`, `Generation`
2. **第二步：实现 MuxClient** — `pick_and_open` (原子) + 状态转换 + 原子计数器
3. **第三步：实现 health_loop + reconnect_loop** — 两阶段 + 子任务模式
4. **第四步：实现 pool_monitor** — JoinSet + 状态感知的 respawn
5. **第五步：简化 mux_client_loop** — 只剩 OpenStream
6. **第六步：删除 main.rs HealthCheck 定时器**
7. **第七步：简化 Message enum** — 只剩 OpenStream
8. **第八步：测试** — 单元测试 + 集成测试 + 故障注入

## 文件改动清单

| 文件 | 改动 |
|------|------|
| `src/tunnel/client.rs` | 大重写（健康逻辑迁移） |
| `src/tunnel/tls_client.rs` | 删除 spawn_tls_replacement；启动时给 monitor 初始连接 |
| `src/tunnel/s2n_quic_client.rs` | 同上 |
| `src/main.rs` | 删除 HealthCheck 定时器 task |
| `src/mux/connection.rs` | 不变（Control::Close 已足够） |

## 测试策略

| 测试类型 | 覆盖点 |
|----------|--------|
| 单元：MuxClient 状态转换 | pick_and_open、mark_retiring、mark_dead、replace_slot 的 generation 校验 |
| 单元：health_loop 状态机 | ping 失败累计到 threshold 触发 mark_retiring |
| 单元：reconnect_loop | backoff 递增；重连成功后 replace_slot |
| 单元：pool_monitor | health task 退出后正确 respawn |
| 集成：mock 连接 | 模拟 ping 失败、模拟重连失败 |
| 故障注入：panic | health task 主动 panic，验证 monitor 兜底 |
| 故障注入：drop | MuxClient::drop 时所有 health task 在 < 1s 内退出 |

## 风险与回退

| 风险 | 缓解 |
|------|------|
| 重构范围大 | 保留原 `health_check` / `retirement_notify` 在分支中；feature flag 切换 |
| 状态机行为变化 | 大量集成测试覆盖（特别是 max_age 路径） |
| 性能影响（ping 频率 N 倍） | 默认 ping_interval = 5s；与当前每秒一次的 health_check 频率相当 |
| generation 数值溢出 | u64 需要 2^64 次替换；实际不会发生 |
