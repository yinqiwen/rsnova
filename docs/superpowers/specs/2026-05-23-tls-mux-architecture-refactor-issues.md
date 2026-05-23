# TLS Mux Architecture Refactor — Review Issues

Spec: `2026-05-23-tls-mux-architecture-refactor.md`

---

## Open Issues

### 1. [LOW] incoming bool 语义重载

`Control::WindowUpdate(sid, increment, incoming)` 中 `incoming` 的含义需要对照其他 Control 变体才能理解。建议实现时考虑拆为：

```rust
WindowUpdateFromPeer(u32, u32),  // credit send_window
WindowUpdateToPeer(u32, u32),    // replenish recv_window + write to wire
```

不影响功能，纯可读性优化。

---

### 2. [LOW] FLAG_AUTH/FLAG_AUTH_ACK/FLAG_OPEN/FLAG_REVERSE_OPEN 处理说明

`read_connection_fut` 中这些 flag 走 `_ =>` 兜底分支（仅 error log）。这是故意的：
- FLAG_AUTH/AUTH_ACK 的数据通过 FLAG_DATA 帧在应用层传输
- FLAG_OPEN/REVERSE_OPEN 的处理是现有逻辑的独立改进项

建议在实现时添加代码注释说明。

---

### 3. [INFO] Backward compatibility strategy

Phase 1 推荐 Option A（协调升级，两端同时部署新版本）。

Option B（优雅降级：30s 超时后回退为 u32::MAX 禁用 flow control）标记为 Future Work。原因：30s 完全停顿比没有 flow control 体验更差，需要更精细的设计（如在 AUTH 握手中协商 flow control 支持）。
