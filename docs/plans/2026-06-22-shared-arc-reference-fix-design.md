# Shared Arc Reference Fix Design

## 背景

v3 self-managed health 重构存在三个 P0 级别的根因相同的 bug：

1. **`replace_slot` 不存储 conn**：调用后 slot 为 Active+None，`pick_and_open` 跳过
2. **`Respawn` 不调用 `install_replacement`**：新 health_loop 获得独占 conn，slot 仍为 None
3. **`Option<T>` 设计分裂所有权**：conn 被 health_loop 独占，pool 侧永远为 None

根因：**health_loop 独占 conn 导致 pool 侧 slot 在 conn 活跃期间永远为 None**。

## 核心方案：共享 Arc 引用

每个 slot 的 `conn` 改为 `Arc<Mutex<Option<T>>>`，pool 和 health_loop 各持一份 clone。

```rust
struct SlotEntry<T> {
    conn: Arc<tokio::sync::Mutex<Option<T>>>,  // 共享引用
    state: SlotState,
    generation: Generation,
}
```

**效果**：
- `pick_and_open` 通过 conn_ref 锁住后调用 `open_stream()`
- `health_loop` 通过 conn_ref 锁住后调用 `ping()`
- 两者通过 slot 内 Mutex 互斥，不会同时操作
- slot 永远持有 conn（除极短暂的替换瞬间），消除 Active+None

## 数据结构变更

### SlotEntry

```rust
// Before:
conn: Option<T>

// After:
conn: Arc<tokio::sync::Mutex<Option<T>>>
```

### MonitorCommand

```rust
// Before:
enum MonitorCommand<T> {
    Respawn { slot: usize, conn: T },
    DropSlot(usize),
}

// After (无泛型):
enum MonitorCommand {
    Respawn { slot: usize },
    DropSlot(usize),
}
```

### 删除 install_replacement

不再需要。新 conn 由 `reconnect_loop` 直接写入 conn_ref。

## API 变更

### push_empty_slot

```rust
// Before:
async fn push_empty_slot(&self) -> usize

// After:
async fn push_empty_slot(&self) -> (usize, Arc<Mutex<Option<T>>>)
```

返回 conn_ref 给调用者（pool_monitor），monitor 负责写入初始 conn 并 clone 给 health_loop。

### replace_slot

```rust
// Before:
async fn replace_slot(&self, slot: usize, new_conn: T, gen_token: Generation)
    -> Result<T, GenMismatch>

// After (不传 conn):
async fn replace_slot(&self, slot: usize, gen_token: Generation)
    -> Result<(), GenMismatch>
```

只更新 state + generation。新 conn 由 `reconnect_loop` 直接写入 conn_ref。

### pick_and_open

```rust
async fn pick_and_open(&self) -> anyhow::Result<(T::SendStream, T::RecvStream)> {
    let guard = self.conns.lock().await;
    for slot in 0..guard.len() {
        let entry = &guard[slot];
        if entry.state != SlotState::Active { continue; }
        let mut conn_guard = entry.conn.lock().await;
        let Some(conn) = conn_guard.as_mut() else { continue; };
        if !conn.is_valid() { continue; }
        return conn.open_stream().await;
    }
    ...
}
```

注意：`conns` 外层锁 + slot 内 conn_ref 锁。锁顺序一致（先外后内），不会死锁。

### mark_dead

不变。但不再需要 `if let Some(conn) = entry.conn.as_mut()` — 改为通过 conn_ref：

```rust
// mark_dead 内 close conn 的方式变更：
{
    let mut conn_guard = entry.conn.lock().await;
    if let Some(mut conn) = conn_guard.take() {
        conn.close();
    }
}
```

## health_loop 变更

### 签名

```rust
// Before:
async fn health_loop<T>(slot, mut conn: T, pool, params, cancel, monitor_tx)

// After:
async fn health_loop<T>(slot, conn_ref: Arc<Mutex<Option<T>>>, pool, params, cancel, monitor_tx)
```

### ping 逻辑

```rust
{
    let mut guard = conn_ref.lock().await;
    let Some(conn) = guard.as_mut() else { continue; };
    if conn.is_valid() {
        if conn.ping().await.is_err() { ... }
    }
}
```

### conn 死亡时

```rust
{
    let mut guard = conn_ref.lock().await;
    if let Some(mut conn) = guard.take() {
        conn.close();
    }
}
pool.mark_dead(slot, gen_token).await;
```

## reconnect_loop 变更

### 签名

```rust
// Before:
async fn reconnect_loop<T>(slot, pool, params, gen_token, cancel) -> MonitorCommand<T>

// After:
async fn reconnect_loop<T>(slot, conn_ref: Arc<Mutex<Option<T>>>, pool, params, gen_token, cancel)
    -> MonitorCommand
```

### 成功时直接写入

```rust
Ok(new_conn) => {
    match pool.replace_slot(slot, gen_token).await {
        Ok(()) => {
            {
                let mut guard = conn_ref.lock().await;
                *guard = Some(new_conn);
            }
            return MonitorCommand::Respawn { slot };
        }
        Err(GenMismatch) => return MonitorCommand::DropSlot(slot),
    }
}
```

**顺序保证**：`replace_slot` 先更新 generation/state（原子），然后写入 conn_ref。
在极短窗口内（replace_slot 成功但 conn_ref 未写入），`pick_and_open` 看到 Active+None，跳过——安全。

## pool_monitor 变更

### 初始连接

```rust
for conn in initial {
    let (slot, conn_ref) = pool.push_empty_slot().await;
    {
        let mut guard = conn_ref.lock().await;
        *guard = Some(conn);
    }
    join_set.spawn(async move {
        let result = tokio::spawn(health_loop(
            slot, conn_ref.clone(), pool_ref, params_ref, cancel_child, monitor_tx_clone,
        )).await;
        (slot, result.map(|_| ()).map_err(|e| e))
    });
}
```

### Respawn 分支

```rust
Some(MonitorCommand::Respawn { slot }) => {
    let conn_ref = {
        let guard = pool.conns.lock().await;
        guard[slot].conn.clone()
    };
    join_set.spawn(async move {
        let result = tokio::spawn(health_loop(
            slot, conn_ref, pool_ref, params_ref, cancel_child, monitor_tx_clone,
        )).await;
        (slot, result.map(|_| ()).map_err(|e| e))
    });
}
```

## P1 修复

### #4: reconnect_limiter().acquire().expect() panic

```rust
// Before:
let _permit = reconnect_limiter().acquire().await.expect("semaphore closed");

// After (加注释说明安全性):
let _permit = reconnect_limiter()
    .acquire()
    .await
    .expect("static semaphore never closes");
```

`reconnect_limiter` 是 `OnceLock<Semaphore>`，没有 `close()` 调用，所以 acquire 永远不会返回 Err。加注释说明即可。

### #5: join_set.join_next() None case

实际上 `join_set.join_next()` 返回 `None` 时 `select!` 不触发，等待其他分支——这是正确行为。加注释说明。

### #6: tls_client.rs url.port().unwrap_or(443)

```rust
// Before:
let remote = (host, url.port().unwrap_or(443))

// After:
let port = url.port_or_known_default()
    .ok_or_else(|| anyhow!("invalid port in URL"))?;
let remote = (host, port)
```

## P2 清理

- 删除 `#[allow(dead_code)]`：方案落地后 `SlotEntry`/`SlotState`/`Generation`/`GenMismatch`/`ConnParams`/`PoolMetrics`/`MonitorCommand` 全部变为活跃代码
- 删除 `install_replacement` 方法及测试
- 删除 `tls_client.rs` 注释掉的死代码
- 更新 `2026-06-16-connection-self-managed-health-design.md` 反映最终设计

## 测试变更

| 测试 | 变更 |
|------|------|
| `replace_slot_increments_generation` | 不再传 `new_conn` 参数 |
| `install_replacement_populates_empty_slot` | 删除（方法已删） |
| `push_empty_increments_active_counter` | 适配 `push_empty_slot` 返回 tuple |
| 新增: `conn_ref_shared_access` | 验证 Arc clone 后 pool 和 health_loop 看到同一个 conn |
| 新增: `pick_and_open_via_conn_ref` | 验证 pick_and_open 通过 conn_ref 工作 |
| 新增: `reconnect_writes_to_conn_ref` | 验证 reconnect_loop 直接写入 conn_ref |
| 新增: `mark_dead_takes_conn_from_ref` | 验证 mark_dead 通过 conn_ref.take() close |

## 并发安全分析

| 场景 | 安全性 |
|------|--------|
| pick_and_open 与 ping 同时访问 conn | slot 内 Mutex 互斥，串行执行 |
| pick_and_open 持锁期间 conn_ref 为 None | 跳过该 slot，安全 |
| replace_slot 成功但 conn_ref 未写入 | pick_and_open 看到 Active+None，跳过；写入是确定性的下一步 |
| mark_dead 与 pick_and_open | conns 外层锁串行化；mark_dead 先 take conn 再设 Dead |
| 两个 health_loop 写同一 conn_ref | 不会发生：旧 health_loop 退出后 monitor 才 spawn 新的 |
| reconnect_loop 写 conn_ref 与 health_loop 读 | slot 内 Mutex 互斥；reconnect_loop 写入时 health_loop 已退出或正在读（互斥） |

## 文件改动清单

| 文件 | 改动 |
|------|------|
| `src/tunnel/client.rs` | SlotEntry.conn → Arc; MonitorCommand 去泛型; 删除 install_replacement; replace_slot 简化; push_empty_slot 返回 tuple; pick_and_open 适配 conn_ref; health_loop 接收 conn_ref; reconnect_loop 接收 conn_ref 并直接写入; pool_monitor 适配; P1 #4 #5 注释; 清理 allow(dead_code); 测试更新 |
| `src/tunnel/tls_client.rs` | P1 #6 port_or_known_default(); 删除注释死代码 |
| `src/tunnel/s2n_quic_client.rs` | 适配 MonitorCommand 无泛型 |
| `docs/plans/2026-06-16-...` | 更新设计文档 |
