# NAT Traversal（反向隧道/内网穿透）

基于现有 mux 通信机制实现内网穿透能力，将内网服务暴露到公网 Server 端口上。

## 使用方式

### Client（内网机器）

带有 `--tunnel` 启用 NAT traversal，否则保持现有 proxy 功能，两者互斥。

> **⚠️ 破坏性变更**：引入 auth stream 后，proxy 模式和 tunnel 模式的连接首条 stream 统一为 auth stream（`FLAG_AUTH`）。**不支持向后兼容**：旧版 client（无 auth stream）无法连接新版 server，反之亦然。升级时 client 和 server 必须同步更新。

```bash
# 基本用法：将内网 1234 端口暴露到公网 1234 端口
./rsnova --role client --remote tls://<server_ip:port> --cert cert.pem \
  --tunnel-client-id office --tunnel 1234

# 指定不同的公网端口
./rsnova --role client --remote tls://<server_ip:port> --cert cert.pem \
  --tunnel-client-id office --tunnel 1234:5678

# 指定内网 IP（非 localhost 的服务）
./rsnova --role client --remote tls://<server_ip:port> --cert cert.pem \
  --tunnel-client-id dc1 --tunnel 192.168.1.100:3306:33306

# 带 SNI：同一公网端口按域名分流到不同内网服务
./rsnova --role client --remote tls://<server_ip:port> --cert cert.pem \
  --tunnel-client-id home \
  --tunnel 192.168.1.10:8080:443:api.example.com \
  --tunnel 192.168.1.20:9090:443:web.example.com

# 混合使用：部分带 SNI，部分不带
./rsnova --role client --remote tls://<server_ip:port> --cert cert.pem \
  --tunnel-client-id office \
  --tunnel 3306:33306 \
  --tunnel 192.168.1.10:8080:443:api.example.com
```

> **协议选择**：示例使用 `tls://`（默认协议）。QUIC 模式（`quic://`）需要编译时启用 `s2n_quic` feature flag。

**`--tunnel` 参数格式：**

| 格式 | 含义 |
|------|------|
| `port` | client 连接 localhost:port，server 暴露 0.0.0.0:port，无 SNI |
| `localPort:remotePort` | client 连接 localhost:localPort，server 暴露 0.0.0.0:remotePort，无 SNI |
| `host:localPort:remotePort` | client 连接 host:localPort，server 暴露 0.0.0.0:remotePort，无 SNI |
| `host:localPort:remotePort:sni` | client 连接 host:localPort，server 暴露 0.0.0.0:remotePort，SNI=sni |

- IPv6 必须用方括号包裹，例如 `[::1]:3306:33306`，避免与多个冒号歧义。
- 协议：仅 TCP（后续可扩展 UDP）。
- SNI 用于同一公网端口上的域名分流。SNI 仅支持合法域名（RFC 1034，不含冒号）。无 SNI 的 tunnel 作为该端口的默认路由（无法识别 SNI 或 SNI 不匹配时使用）。
- 纯端口格式（如 `1234`）的 `local_addr` 默认为 `localhost:1234`（即 `TunnelEntry.local_addr = "localhost:{port}"`）。

**`--tunnel` 参数解析算法：**

1. 若字符串以 `[` 开头：匹配 `]` 提取 IPv6 host，剩余部分按 `:` 分割为 `localPort:remotePort[:sni]`
2. 否则按 `:` 分割全部字段：
   - 1 段 → `port`（`local_addr = "localhost:{port}"`, `remote_port = port`, `sni = None`）
   - 2 段 → `localPort:remotePort`（`local_addr = "localhost:{localPort}"`, `remote_port = remotePort`, `sni = None`）
   - 3 段 → `host:localPort:remotePort`（`local_addr = "{host}:{localPort}"`, `remote_port = remotePort`, `sni = None`）
   - 4 段 → `host:localPort:remotePort:sni`（`local_addr = "{host}:{localPort}"`, `remote_port = remotePort`, `sni = Some(sni)`）
3. Client 端在解析时校验：端口号范围 1-65535、SNI 不含冒号；格式错误直接退出并提示
- `--tunnel` 启用后，以下 client 端参数失效并由 clap `conflicts_with` 强制拒绝：
  `--listen`、`--tproxy`。
  admin server 仍启动（`/metrics` 可用）。
- `--remote` 在 tunnel 模式下仍为**必须参数**，指向公网 server 地址。
- `--concurrent` 在 tunnel 模式下控制到 server 的 mux 连接池大小，所有 tunnel 共享连接池。N 条连接中只要 ≥1 条注册成功即视为 client 可用；注册失败的连接 log warn 后不重试（依赖后续 health_check 重连机制）。
- `--tunnel-client-id`：指定 client 标识前缀（必需），server 据此区分不同 Client 实例。同一 Client 的所有 mux 连接共享相同 `client_id`。详见 [Client 标识](#client-标识client_id) 章节。

### Server（公网机器）

```bash
# 需指定允许的端口范围
./rsnova --role server --listen 0.0.0.0:48100 \
  --protocol tls --key key.pem --cert cert.pem \
  --tunnel-port-range 8000-9000
```

- 收到 Client 的隧道注册请求后，在指定端口上监听（绑定 `0.0.0.0:端口`）
- `--tunnel-port-range`：限制 Client 可以请求的公网端口范围，防止误用。**支持多区间**，逗号分隔：`--tunnel-port-range 8000-9000,10000-10100`。区间为**闭区间**（含两端，即 `[8000, 9000]`）。**禁止 `<1024` 特权端口**，server 启动时校验。
- **未配置 `--tunnel-port-range` 时，server 拒绝所有 tunnel 注册请求**（tunnel 功能不启用）。
- `remote_port` 必须由 Client 显式指定，不支持自动分配（`remote_port=0` 将被 server 拒绝）。
- 端口冲突或越界：注册失败，`TunnelResult.success=false` 并附原因，Client 端打印错误。
- **SNI 路由**：同一 `remote_port` 可注册多条带不同 SNI 的 tunnel，server accept 连接后 peek TLS ClientHello 中的 SNI，按域名路由到对应 tunnel。无 SNI 或未知 SNI 的连接路由到该端口的无 SNI 默认 tunnel（如有），否则关闭连接。
- 无 SNI 的 tunnel 在同一 `remote_port` 上唯一（默认路由），不同 SNI 的 tunnel 在同一端口上互不冲突。
- 多 Client 端口冲突：不同 `client_id` 注册相同 `(remote_port, sni=None)` 时，**先到先得**；SNI 不同则不冲突。
- 自环防护：server 拒绝注册自身已在使用的端口（含 `--listen`、`--admin-listen` 以及其他 tunnel 已占的端口）。
- 认证：见下方 [安全性](#安全性) 章节（第一版沿用现有单向认证）。

### 多 Client 支持

Server 同时接受**多个不同 Client 实例**连接，每个 Client 独立注册和管理自己的隧道。

```
Client A (内网办公室)                Server (公网)
  --tunnel 80:8080         ───────►  :8080 → Client A localhost:80
  --tunnel 3306:33306      ───────►  :33306 → Client A localhost:3306

Client B (内网机房)
  --tunnel 8080:9080       ───────►  :9080 → Client B localhost:8080
  --tunnel 5432:54320      ───────►  :54320 → Client B localhost:5432

Client C (家庭网络)                     :443 (SNI 路由)
  --tunnel 192.168.1.10:8080:443:api.example.com  ──►  api.example.com → Client C :8080
  --tunnel 192.168.1.20:9090:443:web.example.com  ──►  web.example.com → Client C :9090
```

**Server 端管理：**

- Server 以 `client_id` 为维度管理隧道，维护 `HashMap<ClientId, ClientState>` 跟踪各 Client 的隧道和连接
- 同一 `client_id` 的多条 mux 连接归为同一个 Client（因为 `--concurrent=N` 会建立 N 条连接）
- 端口 + SNI 全局唯一：不同 `(remote_port, sni)` 组合互不冲突；相同 `(remote_port, sni)` 先到先得
- **端口级共享**：同一 `remote_port` 的 listener 由该端口上所有 SNI 路由共享，不归属于单个 `client_id`。仅当某端口上所有 SNI 路由均被清理后，才关闭 listener 释放端口
- Client 断开时，**立即清理该 client_id 注册的所有路由**。若某端口上仍有其他 client 的路由，listener 继续运行

**Client 标识（client_id）：**

- 由 `--tunnel-client-id` 命令行参数指定，格式为用户自定义字符串前缀，如 `office`、`dc1-mysql`、`home-ssh`
- 最终 `client_id` = `<前缀>`，直接用于 server 端归组
- 同一 Client 的所有 mux 连接（`--concurrent=N`）携带相同 `client_id`
- 不同 Client 实例必须指定不同前缀，由用户保证唯一性。若两个 Client 使用相同 `client_id`，server 会将它们视为同一 client 归组，导致反向 stream 可能被路由到错误的 Client 连接上
- 断线重连：重连时使用相同 `--tunnel-client-id` → 相同 `client_id` → server 识别为同一 client
- 日志和 metrics 中使用 `client_id` + 远端地址作为标识
- Admin server `/metrics` 端点按 `client_id` 维度展示各隧道状态

**Tunnel Metrics 指标定义：**

| 指标名 | 类型 | 标签 | 说明 |
|--------|------|------|------|
| `tunnel_active_routes` | gauge | `client_id`, `remote_port`, `sni` | 当前活跃路由数 |
| `tunnel_active_connections` | gauge | `client_id` | 某 client 的存活 mux 连接数 |
| `tunnel_visitor_connections` | gauge | `remote_port`, `sni` | 当前访客连接数 |
| `tunnel_visitor_total` | counter | `remote_port`, `sni` | 累计访客连接数 |
| `tunnel_register_total` | counter | `client_id`, `result` | 累计注册次数（result: success/failure） |

**多连接注册流程：**

- Client 的 N 条 mux 连接并发建立，每条连接建立后独立通过 auth stream 发送 `FLAG_AUTH` + `AuthRequest::Register`（携带相同 `client_id`）
- Server 对每条连接都走完整的 auth stream 流程（读 `FLAG_AUTH` → 校验 → 回复 `FLAG_AUTH_ACK` → auth stream 关闭）
- 首次收到某 `client_id` 的 `AuthRequest::Register`：校验端口、bind、建立 `client_id` → 端口映射
- 收到同 `client_id` 的后续 `AuthRequest::Register`：已注册的端口不重复 bind（幂等），仅将该连接加入该 client 的可用连接池
- 外部访客连接时，server 在该 tunnel 对应 `client_id` 的所有可用连接中**轮询**选一条发起 `FLAG_REVERSE_OPEN` stream

## 协议交互

### 数据流对比

**现有 proxy 模式：**
```
用户浏览器 → client:listen → [mux stream] → server → 目标网站
             (client 发起 FLAG_OPEN)
```

**NAT traversal 模式（反向）：**
```
外部访客 → server:公网端口 → [mux 反向 stream] → client → 内网服务
            (server 发起 FLAG_REVERSE_OPEN)
```

核心差异：proxy 模式由 client 侧发起 `FLAG_OPEN`；tunnel 模式由 **server 侧通过已有 mux 连接向 client 发起反向 stream，首帧为 `FLAG_REVERSE_OPEN`**。之后一律使用 `FLAG_DATA` 双向 relay，与 proxy 模式完全对称。

### 交互时序（TLS mux 模式）

**Proxy 模式：**

```
Client                              Server
  |                                    |
  |--- [建立 mux 连接(TLS)] -------->|
  |                                    |
  |  open_stream() --------------->   |  accept_stream() → SYN
  |  FLAG_AUTH + AuthRequest::Proxy ->|  (auth stream：proxy 模式)
  |  <-- FLAG_AUTH_ACK + AuthAck::Proxy |  auth stream 握手完成，关闭
  |                                    |
  |  open_stream() --------------->   |  accept_stream()
  |  写入 FLAG_OPEN + addr -------->  |  (data stream：与现有实现完全一致)
  |                                    |  handle_server_stream() → 连接目标
  |<========= FLAG_DATA 双向 relay ===>|
  |                                    |
  |  ... 后续 data stream 同上 ...     |
```

**Tunnel 模式：**

```
Client                              Server
  |                                    |
  |--- [建立 mux 连接(TLS)] -------->|
  |                                    |
  |  open_stream() --------------->   |  accept_stream() → SYN
  |  FLAG_AUTH + AuthRequest::        |  (auth stream：tunnel 模式)
  |    Register{client_id, tunnels} ->|  server 校验端口范围、冲突
  |                                    |  server bind 公网端口
  |  <-- FLAG_AUTH_ACK + AuthAck::  --|  (通过 auth stream 回复)
  |        RegisterAck{results}       |  auth stream 握手完成，关闭
  |                                    |  保存 mux::Connection 引用到 TunnelRegistry
  |                                    |
  |  ... 等待外部访客 ...              |  ... server 持有 connection 引用，等待访客触发 open_stream() ...
  |                                    |
  |                        外部访客 --> | :公网端口 (TCP accept)
  |                                    |  (若有 SNI 路由：peek_sni_v2 提取 ClientHello SNI)
  |                                    |  (根据 SNI 查找对应 tunnel)
  |  <--------- accept_stream() ─────|  server open_stream() → client 收到新 stream
  |  读首帧 FLAG_REVERSE_OPEN         |
  |  解码 OpenStreamEvent{addr}       |
  |  连接 addr 指定的内网服务          |
  |                                    |
  |<========= FLAG_DATA 双向 relay ===>|
  |                                    |
```

### 交互时序（QUIC 模式）

QUIC 模式无 mux 层，每个 QUIC bidirectional stream 独立。首条 stream 为 auth stream：

**Proxy 模式：**

```
Client                              Server
  |                                    |
  |--- [建立 QUIC 连接] ------------>|
  |                                    |
  |  open_bidirectional_stream() -->  |  accept_bidirectional_stream()
  |  FLAG_AUTH + AuthRequest::Proxy ->|  (auth stream：proxy 模式)
  |  <-- FLAG_AUTH_ACK + AuthAck::Proxy |  auth stream 握手完成，关闭
  |                                    |
  |  open_bidirectional_stream() -->  |  accept_bidirectional_stream()
  |  写入 FLAG_OPEN + addr -------->  |  (data stream：与现有实现完全一致)
  |                                    |  handle_server_stream() → 连接目标
  |<========= FLAG_DATA 双向 relay ===>|
  |                                    |
```

**Tunnel 模式：**

```
Client                              Server
  |                                    |
  |--- [建立 QUIC 连接] ------------>|
  |                                    |
  |  open_bidirectional_stream() -->  |  accept_bidirectional_stream()
  |  FLAG_AUTH + AuthRequest::        |  (auth stream：tunnel 模式)
  |    Register{client_id, tunnels} ->|  server 校验、bind
  |  <-- FLAG_AUTH_ACK + AuthAck::  --|  (在同一条 stream 上回复)
  |        RegisterAck{results}       |  auth stream 握手完成，关闭
  |                                    |
  |  connection.split() →             |  connection.split() →
  |    (Handle, StreamAcceptor)        |    (Handle, StreamAcceptor)
  |                                    |  保存 Handle 到 TunnelRegistry
  |                                    |
  |         ... 等待外部访客 ...        |
  |                                    |
  |                        外部访客 --> | :公网端口 (TCP accept)
  |                                    |  (若有 SNI 路由：peek_sni_v2 提取 ClientHello SNI)
  |  <--- new bidirectional stream ---|  handle.clone().open_bidirectional_stream()
  |  (client StreamAcceptor 收到)      |
  |  读首帧 FLAG_REVERSE_OPEN         |
  |  解码 OpenStreamEvent{addr}       |
  |  连接 addr 指定的内网服务          |
  |                                    |
  |<========= FLAG_DATA 双向 relay ===>|
  |                                    |
```

> **QUIC 模式说明**：
> - Server 端：s2n-quic 支持 server 端主动 `open_bidirectional_stream()`（QUIC 协议本身允许），需改造 `start_quic_remote_server` 在 tunnel 模式下使用 `open_bidirectional_stream()` 发起反向 stream
> - Client 端：当前 QUIC client 仅主动 `open_bidirectional_stream()`，tunnel 模式需新增 `accept_bidirectional_stream()` 循环接收 server 发起的反向 stream

### 新增消息/事件类型

在现有 `mux/event.rs` 的 flag 体系中新增。完整 flag 值分配表：

| Flag | 值 | 方向 | 说明 |
|------|---|------|------|
| `FLAG_OPEN` | 1 | Client → Server | data stream 首帧（proxy 模式），payload 为 `OpenStreamEvent` |
| `FLAG_FIN` | 2 | 双向 | stream 关闭 |
| `FLAG_DATA` | 3 | 双向 | 数据传输 |
| `FLAG_SYN` | 4 | 双向 | mux stream 握手（connection 层控制帧） |
| `FLAG_PING` | 5 | Client → Server | 心跳 |
| **`FLAG_AUTH`** | **6** | **Client → Server** | **auth stream 首帧，payload 为 `AuthRequest`（统一携带模式 + 注册信息）** |
| `FLAG_SHUTDOWN` | 7 | 双向 | stream shutdown |
| `FLAG_PONG` | 8 | — | （预留值，从未使用） |
| **`FLAG_AUTH_ACK`** | **9** | **Server → Client** | **auth stream 响应，payload 为 `AuthAck`（统一携带模式确认 + 注册结果）** |
| **`FLAG_REVERSE_OPEN`** | **10** | **Server → Client** | **反向 stream 首帧，payload 复用 `OpenStreamEvent`，client 据此连接内网服务** |

> **值分配说明**：新增 flag 使用 6、9、10，与现有 flag（7=FLAG_SHUTDOWN, 8=预留）无冲突。

> **⚠️ 破坏性变更**：引入 auth stream 后，所有连接（含 proxy 模式）首条 stream 必须为 auth stream。旧版 client 直接发送 `FLAG_OPEN` 会被新版 server 拒绝（首帧 flag 不为 `FLAG_AUTH`）。**不支持新旧版本混部**，client 和 server 必须同步升级。

> 设计说明：
> - `FLAG_AUTH` / `FLAG_AUTH_ACK` 统一了 proxy 和 tunnel 两种模式的认证流程，不按模式拆分 flag。模式由 payload 中的枚举区分。
> - tunnel data stream 与 proxy stream 使用相同的 `FLAG_DATA` 传输数据，无需额外 flag。
> - `FLAG_REVERSE_OPEN` 复用 `OpenStreamEvent` 作为首帧 payload，与 proxy 模式完全对称：
>   - Proxy：client `FLAG_OPEN` + `OpenStreamEvent{proto, addr}` → server 连接 addr
>   - Tunnel：server `FLAG_REVERSE_OPEN` + `OpenStreamEvent{proto, addr}` → client 连接 addr
>   - 无需新增 payload 结构体，client 侧处理逻辑也可复用。
> - 无需 Deregister 消息。Auth stream 握手后即关闭，Client 退出时直接断开 mux/QUIC 连接，
>   Server 通过连接断开检测清理路由。

**Auth payload（bincode 编码）：**

```rust
#[derive(Encode, Decode)]
enum AuthRequest {
    Proxy,                                  // 声明 proxy 模式，无额外数据
    Register(RegisterRequest),              // 声明 tunnel 模式 + 注册隧道列表
}

#[derive(Encode, Decode)]
struct RegisterRequest {
    client_id: String,          // --tunnel-client-id 指定的前缀，用于归组同一 client 的多条连接
    tunnels: Vec<TunnelEntry>,
}

#[derive(Encode, Decode)]
struct TunnelEntry {
    local_addr: String,     // client 侧目标地址，如 "localhost:3306"（与 OpenStreamEvent.addr 格式一致）
    remote_port: u16,       // 请求的 server 公网端口（必须显式指定，0=无效）
    sni: Option<String>,    // SNI 域名，如 "api.example.com"；None 表示该端口的默认路由
}
```

**AuthAck payload：**

```rust
#[derive(Encode, Decode)]
enum AuthAck {
    Proxy,                                  // proxy 模式确认
    RegisterAck(RegisterAck),               // tunnel 模式注册结果
}

#[derive(Encode, Decode)]
struct RegisterAck {
    results: Vec<TunnelResult>,
}

#[derive(Encode, Decode)]
struct TunnelResult {
    success: bool,
    remote_port: u16,               // 回显请求的端口
    sni: Option<String>,            // 回显请求的 SNI
    error: Option<String>,          // 失败原因
}
```

**ReverseOpenStream payload（data stream 首帧）：复用 `OpenStreamEvent`**

`FLAG_REVERSE_OPEN` 的 payload 直接复用现有 `OpenStreamEvent { proto, addr }`，无需新增结构体。server 在 addr 中填入 `TunnelEntry.local_addr`，client 解码后连接该地址即可。

> 注意：现有 `new_open_stream_event()` 函数内部写死了 `FLAG_OPEN`，需新增 `new_reverse_open_stream_event()` 函数，逻辑相同但使用 `FLAG_REVERSE_OPEN`。

### Auth Stream 与模式分发

**核心原则：每个 connection（TLS mux / QUIC）的第一条 stream 固定为 auth stream，其首帧 event 的 flag 为 `FLAG_AUTH`，payload 中的 `AuthRequest` 枚举标记连接模式（proxy 或 tunnel）。**

这样设计的好处：
- **mux 层简洁**：只需 `FLAG_AUTH` / `FLAG_AUTH_ACK` 两个 flag，模式区分在 payload 枚举层面
- **职责分离**：模式判断归 auth stream，业务数据归 data stream，不混杂
- **auth stream 短生命周期**：握手完成后即可关闭，不占用额外资源
- **data stream 无需改动**：proxy 模式的 data stream 首帧仍是 `FLAG_OPEN` + `OpenStreamEvent`，与现有实现完全一致
- **可扩展**：未来加新模式只需在 `AuthRequest` / `AuthAck` 枚举中加变体，mux 层 flag 不变

**Auth stream 首帧 event：**

```
flag = FLAG_AUTH (6), ack = FLAG_AUTH_ACK (9)
payload = AuthRequest 枚举（bincode 编码）
  ├─ AuthRequest::Proxy          → proxy 模式
  └─ AuthRequest::Register(...)  → tunnel 模式 + 注册隧道列表
```

**Auth stream 后续行为：**

| 模式 | Auth stream 生命周期 | 后续 data stream |
|------|---------------------|-----------------|
| Proxy | Client 发送 `FLAG_AUTH` + `AuthRequest::Proxy` → Server 回复 `FLAG_AUTH_ACK` + `AuthAck::Proxy` → **auth stream 关闭** | Client 主动 `open_stream()`，首帧 `FLAG_OPEN` + `OpenStreamEvent`，与现有实现完全一致 |
| Tunnel | Client 发送 `FLAG_AUTH` + `AuthRequest::Register(...)` → Server 回复 `FLAG_AUTH_ACK` + `AuthAck::RegisterAck(...)` → **auth stream 关闭** | Server 主动 `open_stream()`，首帧 `FLAG_REVERSE_OPEN` + `OpenStreamEvent` |

**Client 连接流程（统一）：**

```
Client 建立到 Server 的连接 (TLS mux / QUIC)
  │
  ├─ open_stream() → FLAG_AUTH + AuthRequest::Proxy 或 AuthRequest::Register
  │  (auth stream，第一条 stream)
  │
  ├─ [Proxy 模式]
  │   收到 FLAG_AUTH_ACK + AuthAck::Proxy，auth stream 关闭
  │   后续 open_stream() → FLAG_OPEN + OpenStreamEvent → handle_server_stream (不变)
  │
  └─ [Tunnel 模式]
      收到 FLAG_AUTH_ACK + AuthAck::RegisterAck(...)，auth stream 关闭
      后续 accept_stream() → FLAG_REVERSE_OPEN → 连接本地服务
```

**Server 连接流程（统一）：**

```
Server accept 新连接 (TLS mux / QUIC)
  │
  ├─ accept_stream() → 读取首帧 event
  │  (auth stream，第一条 stream)
  │
  ├─ FLAG_AUTH + AuthRequest::Proxy → 标记该连接为 proxy 模式
  │   回复 FLAG_AUTH_ACK + AuthAck::Proxy，auth stream 关闭
  │   后续 accept_stream() → handle_server_stream (不变)
  │
  └─ FLAG_AUTH + AuthRequest::Register → 标记该连接为 tunnel 模式
      解码 RegisterRequest → 校验 → 回复 FLAG_AUTH_ACK + AuthAck::RegisterAck(...)，auth stream 关闭
      保存 connection 引用到 TunnelRegistry
      连接断开时触发该 client 全量路由清理
```

> **设计说明**：
> - Auth stream 不阻塞 data stream。Client 写完首帧后可**连续开 data stream**，不必等 Server 确认。
>   Server 按 accept 顺序处理：先读到 auth stream 确立模式，再处理后续 data stream。
>   QUIC 中 stream 按 ID 排序，TLS mux 中 `accept_stream()` 也是 FIFO，顺序有保证。
> - 两种模式 auth stream 都有 ACK：`FLAG_AUTH_ACK` + `AuthAck`。Proxy 模式的 ACK 是轻量的确认，
>   Tunnel 模式的 ACK 承载注册结果。统一 ACK 便于实现，且 proxy 模式的 ACK 开销可忽略。
> - Auth stream 握手完成后即关闭，不保持长连接。Client 正常退出直接断开 mux/QUIC 连接，
>   Server 通过连接断开检测清理路由，无需额外的 Deregister 消息。

### Auth Stream 与 Data Stream

**Auth stream（连接首条 stream）：**
- 每个连接的第一条 stream 固定为 auth stream，首帧 event flag 为 `FLAG_AUTH`，payload 为 `AuthRequest` 枚举（`Proxy` / `Register`）
- 握手完成后 auth stream 即关闭：Client 收到 `FLAG_AUTH_ACK` 后关闭，Server 发送 `FLAG_AUTH_ACK` 后关闭
- Auth stream 不承载心跳或其他长连接消息，生命周期仅限于握手阶段

**Data stream：**
- 每个外部访客连接对应一条新的 stream，由 **server 侧发起**
- **TLS mux 模式**：server 侧 `open_stream()` 创建，首帧写入 `FLAG_REVERSE_OPEN` + `OpenStreamEvent`
- **QUIC 模式**：server 侧通过 `Handle`（由 `Connection::split()` 得到）调用 `handle.open_bidirectional_stream()` 创建，首帧写入 `FLAG_REVERSE_OPEN` + `OpenStreamEvent`
- Client 收到新 stream 后读取首帧 `FLAG_REVERSE_OPEN`，解码 `OpenStreamEvent`，连接 `addr` 指定的内网服务，进入双向 relay
- Data stream 首帧之后一律使用 `FLAG_DATA`，与 proxy 模式一致

**Client 侧流程（与 proxy 模式完全独立）：**

Tunnel 模式不复用 proxy 的 `mux_client_loop`，而是独立的运行流程：

1. **Auth stream**（client 主动创建的第一条 stream）：发送 `FLAG_AUTH` + `AuthRequest::Register(...)`，等待 `FLAG_AUTH_ACK` + `AuthAck::RegisterAck(...)`，握手完成后 auth stream 关闭
2. **Data stream 接收循环**（server 侧发起的新 stream）：
   - TLS mux 模式：持续 `accept_stream()` → 收到新 stream → 读取首帧 `FLAG_REVERSE_OPEN` → 解码 `OpenStreamEvent` → 连接本地服务 → 双向 relay
   - QUIC 模式：持续 `accept_bidirectional_stream()` → 同上
3. 两类 stream 在不同的 tokio task 中独立处理，互不干扰

### SNI 路由机制

当同一 `remote_port` 注册了多条带不同 SNI 的 tunnel 时，server 的 accept 循环需进行 SNI 路由：

1. Server accept 外部 TCP 连接
2. Peek 前 N 字节，尝试解析 TLS ClientHello 中的 SNI（复用现有 `tls_local.rs` 中的 `peek_sni_v2` 函数，其接受 `&TcpStream` 参数，适合非消费性读取）
3. 若提取到 SNI 且匹配已注册的 `(remote_port, Some(sni))` tunnel → 路由到该 tunnel
4. 若 SNI 未匹配或连接非 TLS → 路由到该端口的默认 tunnel `(remote_port, None)`
5. 若无匹配的 SNI tunnel 且无默认 tunnel → 关闭连接

> 注意：SNI peek 是**非消费性读取**（`TcpStream::peek` 而非 read），外部连接的原始字节需完整转发给 client，
> server 不做 TLS 终止，仅 peek SNI 做路由判断。

## 连接生命周期

### 启动与注册

1. Client 建立到 Server 的 mux 连接（复用现有 QUIC/TLS 建连逻辑）
2. Client 在 auth stream 上发送 `FLAG_AUTH` + `AuthRequest::Register(...)` 消息
3. Server 校验并 bind 端口，回复 `FLAG_AUTH_ACK` + `AuthAck::RegisterAck(...)`
4. Client 收到成功响应后进入就绪状态，打印隧道映射信息
5. 部分 tunnel 注册失败时：成功的隧道正常运行，失败的打印错误。若**所有** tunnel 均失败则 Client 退出。

### 断线重连

- Client 检测到 mux 连接断开后，自动重连（复用现有 health_check 机制）
- **重连成功后 Client 自动重发 `FLAG_AUTH` + `AuthRequest::Register(...)`**（携带相同 `client_id`），server 识别为同一 client 回来：若路由仍存活（其他连接还在）则幂等处理不重复 bind；若路由已被清理则重新 bind 端口（`SO_REUSEADDR` 保证端口可立即 re-bind）
- Server 端在检测到某条 mux 连接断开后：
  - 从该 `client_id` 的连接池中移除该连接
  - 若该 `client_id` 仍有其他存活连接：隧道正常运行，仅减少可用连接数
  - 若该 `client_id` 所有连接均断开：**立即清理该 client 注册的所有路由**
  - 清理路由后，若某端口上仍有其他 client 的路由，listener 继续运行；若端口上所有路由均已清理，则关闭 listener 释放端口
- Register 幂等：server 收到同 `client_id` 的重复 `AuthRequest::Register`（如网络重传/重连竞态），已注册的端口不重复 bind，仅将新连接加入连接池

**断线重连的竞态处理：**

Client A 所有连接断开 → server 开始清理 A 的路由 → A 立即重连发 Register。清理和注册可能并发执行，导致端口 re-bind 失败或重复 bind。解决方案：
- `TunnelRegistry` 的注册和清理操作需互斥：使用 `tokio::sync::RwLock` 或 `Mutex` 保护，确保同 `client_id` 的清理和注册不会并发执行
- 清理操作完成后才处理新 Register：若清理和 Register 请求同时到达，Register 应等待清理完成后再执行，此时端口已释放，可正常 re-bind（`SO_REUSEADDR` 保证端口可立即 re-bind）

### 心跳与保活

- Tunnel 模式复用现有 `FLAG_PING` 心跳机制：client 定期向 server 发送 PING，server 回复（与 proxy 模式一致）
- **TLS mux 模式**：`mux::Connection` 内置的 ping/pong 机制自动工作，tunnel 模式无需额外处理
- **QUIC 模式**：QUIC 协议内置 keep-alive（s2n-quic 支持配置 idle timeout 和 keep-alive interval），由传输层自动维持连接活跃
- `--idle-timeout-secs` 参数在 tunnel 模式下仍生效，控制连接超时时间。建议 tunnel 场景适当调大（如 300s），避免无访客流量时连接被回收
- 连接断开检测：TLS mux 模式通过 ping 超时检测，QUIC 模式通过协议层 idle timeout 检测

### 正常退出（Graceful Shutdown）

- Client 正常退出时直接断开 mux/QUIC 连接
- Server 通过连接断开检测清理该 client 的所有路由，若某端口因此无路由则关闭 listener
- 无需额外的 Deregister 消息：auth stream 在握手后已关闭，断开连接本身就是注销信号

### 异常处理

- Client 收到 `FLAG_REVERSE_OPEN` 但 `OpenStreamEvent.addr` 无法连接：关闭该 stream，记录 warn 日志
- Client 收到新 stream 但首帧不是 `FLAG_REVERSE_OPEN`：关闭 stream，记录 warn 日志
- SNI peek 失败（数据太短、非 TLS）：回退到默认 tunnel，若无默认 tunnel 则关闭连接

### 并发限制

暂不限制单个 Client 的最大隧道数和 Server 总最大监听端口数（受 `--tunnel-port-range` 范围自然约束）。

## 实现要点

### Client 侧

1. 解析 `--tunnel` 参数，构建 `TunnelEntry` 列表
2. 建立 mux 连接后，主动 `open_stream()` 创建 auth stream，发送 `FLAG_AUTH` + `AuthRequest::Register(...)`
3. 等待 `FLAG_AUTH_ACK` + `AuthAck::RegisterAck(...)`，处理失败情况（部分成功继续，全部失败退出）
4. 持续 `accept_stream()` / `accept_bidirectional_stream()` 接收 server 侧发起的 data stream：
   - 读取首帧，若为 `FLAG_REVERSE_OPEN`：解码 `OpenStreamEvent`，连接 `addr` 指定的内网服务，进入双向 relay
   - 连接本地服务失败：关闭 stream（发 FIN）

### Server 侧

1. 解析 `--tunnel-port-range`，构建允许端口集合（支持多区间）；校验无特权端口
2. 维护全局状态：`TunnelRegistry`（端口 + SNI 分配表 + 各 Client 连接映射）
3. 收到新的 mux 连接后，spawn 独立任务处理该 Client：
   a. `accept_stream()` 一次得到第一条 stream → 视为 auth stream
   b. 读取首帧 event（flag = `FLAG_AUTH`），解码 `AuthRequest` 判断模式：
      - `AuthRequest::Proxy` → proxy 模式，回复 `FLAG_AUTH_ACK` + `AuthAck::Proxy`，auth stream 关闭，后续 stream 走 `handle_server_stream`（与现有实现一致）
      - `AuthRequest::Register` → tunnel 模式，解码 `RegisterRequest`，校验 `TunnelEntry`：端口范围、`(remote_port, sni)` 冲突、自环（含 `--listen`、`--admin-listen` 已用端口）
   c. 首次注册某 `remote_port` 时执行 `TcpListener::bind`；已有同端口 listener 则复用
   d. 回复 `FLAG_AUTH_ACK` + `AuthAck::RegisterAck(...)`（含部分成功/失败结果）
   e. 此后保留 `mux::Connection` 引用到 `TunnelRegistry`（供 accept 访客时 `open_stream()`）
   f. 连接断开时触发该 client 全量路由清理
4. 为每个 `remote_port` spawn 一个 accept 循环（所有 SNI tunnel 共享同一 listener）：
   - accept 到新连接后，peek SNI（复用 `tls_local.rs` 的 `peek_sni_v2`）
   - 根据 `(remote_port, sni)` 查找对应 tunnel，获取 `client_id` 和 `local_addr`
   - 在该 `client_id` 的连接池中轮询选一条 mux 连接
   - 在该 mux 连接上 `open_stream()`，写入 `FLAG_REVERSE_OPEN` + `OpenStreamEvent{proto:"tcp", addr:local_addr}` 作为首帧
   - 直接在 data stream 与外部 TCP 连接之间双向 relay（使用 `FLAG_DATA`）
5. Client 连接断开时：清理该 client 的所有路由；若某端口所有路由均清空则关闭 listener

**Server 端连接处理完整控制流（`handle_tls_connection` / `handle_quic_connection` 改造）：**

```
Server accept 新连接
  │
  ├─ accept_stream() → 得到第一条 stream（auth stream）
  │   读取首帧 event:
  │
  ├─ [FLAG_AUTH + AuthRequest::Proxy]
  │   回复 FLAG_AUTH_ACK + AuthAck::Proxy → auth stream 关闭
  │   进入 proxy 模式 accept_stream() 循环（与现有实现一致）
  │
  └─ [FLAG_AUTH + AuthRequest::Register]
      校验 → 回复 FLAG_AUTH_ACK + AuthAck::RegisterAck → auth stream 关闭
      TLS: 保存 mux::Connection 引用到 TunnelRegistry
      QUIC: connection.split() → 保存 Handle 到 TunnelRegistry
      进入 tunnel 模式：等待连接断开
      （连接断开 = client 退出信号 → 触发该 client 全量路由清理）
```

> **注意**：tunnel 模式下，server 端连接处理 task 在 auth stream 完成后不再 accept 新 stream。连接的用途是：TLS 模式持有 `mux::Connection`（`Arc` 共享）供 visitor handler 调用 `open_stream()`；QUIC 模式持有 `StreamAcceptor` 检测连接断开，`Handle` 已 clone 到 TunnelRegistry 供 visitor handler 调用 `open_bidirectional_stream()`。

**TunnelRegistry 核心结构（概念）：**

```rust
/// 路由键：remote_port + SNI
#[derive(Hash, Eq, PartialEq)]
struct RouteKey {
    remote_port: u16,
    sni: Option<String>,   // None = 默认路由
}

struct TunnelRegistry {
    /// (remote_port, sni) → 隧道信息
    routes: HashMap<RouteKey, ActiveTunnel>,
    /// remote_port → 端口状态（listener + SNI 路由表）
    /// 端口级共享：不归属于单个 client_id，生命周期由该端口所有路由共同决定
    ports: HashMap<u16, PortState>,
    /// client_id → 该 Client 的状态（连接池 + 注册路由列表）
    clients: HashMap<String, ClientState>,
}

struct PortState {
    listener: TcpListener,
    listener_handle: JoinHandle<()>,
    cancel_token: CancellationToken,
    /// 该端口上已注册的路由键列表（含 None 默认路由）
    /// 当此列表为空时，关闭 listener 释放端口
    active_routes: Vec<RouteKey>,
}

struct ClientState {
    connections: Vec<ClientConnection>,
    routes: Vec<RouteKey>,                   // 该 client 注册的路由列表
    cursor: usize,                           // 轮询游标
}

struct ClientConnection {
    handler: ConnectionHandler,
    conn_id: u32,       // 连接标识，用于日志
}

/// Server 端持有的 client 连接抽象
enum ConnectionHandler {
    /// TLS 模式：mux::Connection 的 open_stream(&self) 通过 mpsc::Sender + AtomicU32 实现，
    /// 天然支持多 task 并发调用，仅需 Arc 共享所有权
    Tls(Arc<mux::Connection>),
    /// QUIC 模式：通过 Connection::split() 得到的 Handle，
    /// Handle 实现 Clone，每个 visitor handler clone 一份后独立调用 open_bidirectional_stream()
    Quic(s2n_quic::connection::Handle),
}

struct ActiveTunnel {
    client_id: String,
    local_addr: String,                    // client 侧目标地址，如 "localhost:3306"
    remote_port: u16,                      // server 侧公网端口
    sni: Option<String>,                   // SNI 域名，None = 默认路由
}
```

**端口级 listener 共享逻辑：**

- `PortState.active_routes` 跟踪该端口上所有活跃路由
- 新路由注册时：添加到 `active_routes`；若端口首次出现则 `TcpListener::bind`
- 路由清理时：从 `active_routes` 移除；若 `active_routes` 为空则 cancel listener + 释放端口
- Client A 断开时，仅移除 A 的路由；若 B 在同一端口仍有路由，listener 不关闭

### TLS 模式 vs QUIC 模式差异

| 维度 | TLS mux 模式 | QUIC 模式 |
|------|-------------|-----------|
| 传输层 | `mux::Connection` over TLS TCP | 原生 QUIC bidirectional stream |
| Auth stream | Client `open_stream()` 创建，mux SYN 握手 | 约定第一条 `open_bidirectional_stream()` |
| Server 反向 stream | `mux::Connection(Mode::Server).open_stream()` | `Handle.open_bidirectional_stream()`（Handle 由 `Connection::split()` 得到） |
| Data stream 数据传输 | `FLAG_DATA`（与 proxy 模式一致） | `FLAG_DATA`（与 proxy 模式一致） |
| Data stream 首帧 | `FLAG_REVERSE_OPEN` + `OpenStreamEvent` | `FLAG_REVERSE_OPEN` + `OpenStreamEvent` |
| SNI 路由 | accept 后 `peek_sni_v2` 提取 TLS ClientHello SNI | 同 TLS 模式（外部连接仍是 TCP） |
| 现有可复用代码 | `tls_remote.rs` 的 `mux::Connection(Mode::Server)` — server 侧已具备 `open_stream()` 能力 | `s2n_quic::connection::Handle`（由 `Connection::split()` 得到，`Clone` 支持多 task） |

### mux 层改造说明

**Server 端 `open_stream()` 能力（TLS mux 模式）：**

当前 `mux::Connection` 的 `open_stream()` 方法在 client/server 两侧均可调用（stream ID 采用奇偶分离：client 使用偶数 ID 0,2,4...，server 使用奇数 ID 1,3,5...，步长为 2），tunnel 模式下 server 端需要调用 `open_stream()` 向 client 发起反向 stream。现有实现中 server 侧仅用 `accept_stream()`，需确认：
- `open_stream()` 在 server mode 下可正常工作（奇偶分离保证 stream ID 不冲突）
- Server 端调用 `open_stream()` 后，client 侧通过 `accept_stream()` 接收（复用现有 SYN 握手机制）

**`open_stream()` 多 task 并发调用安全性：** `mux::Connection::open_stream()` 内部仅使用 `self.ev_writer.send(Control::NewStream(...))`（`mpsc::Sender` 线程安全）和 `self.stream_id_seed.fetch_add(2, Ordering::SeqCst)`（原子操作），因此多个 tokio task 可安全地同时调用同一 `mux::Connection` 的 `open_stream()`，无需额外同步。

**`accept_stream()` 不可并发调用约束：** `mux::Connection::accept_stream()` 内部使用 `oneshot::channel` 回调模式，一次只允许一个 pending accept。若两个 task 同时调用 `accept_stream()`，第二个会收到 "duplicate accept" 错误。因此 tunnel client 必须确保**仅一个 task** 负责调用 `accept_stream()` 循环。

**QUIC 模式 server 端 `open_bidirectional_stream()` 能力：**

- s2n-quic：`s2n_quic::connection::Connection` 提供 `open_bidirectional_stream()`，QUIC 协议原生支持 server 主动发起 stream（server-initiated streams 使用偶数 stream ID）。当前项目使用的 s2n-quic 版本已支持此 API。

**QUIC 模式 `Connection` 的 `&mut self` 共享问题：**

s2n-quic 的 `open_bidirectional_stream()` 和 `accept_bidirectional_stream()` 均要求 `&mut self`。TLS mux 模式无此问题（`mux::Connection` 内部用 channel 通信，`open_stream()` / `accept_stream()` 均不要求 `&mut self`）。

**解决方案：`Connection::split()`**（推荐，无需 `Arc<Mutex<>>`）

s2n-quic 提供 `Connection::split(self) -> (Handle, StreamAcceptor)`，将连接拆分为两个独立部分，各自可在不同 tokio task 中使用：

| 类型 | 用途 | 方法 | 线程安全 |
|------|------|------|---------|
| `Handle` | 打开流（server 发起反向 stream） | `handle.open_bidirectional_stream(&mut self)` | `Clone + Send + Sync`，可 clone 到多个 task 各自独立调用 |
| `StreamAcceptor` | 接收流（接收 auth stream 等） | `acceptor.accept_bidirectional_stream(&mut self)` | `Send + Sync`，独占使用 |

**Server 侧架构**：

```rust
// auth 阶段：先在完整 Connection 上做 auth stream 握手
let first_stream = connection.accept_bidirectional_stream().await?;
// ... 读取 FLAG_AUTH，处理注册 ...

// auth 完成后 split
let (handle, mut acceptor) = connection.split();

// Handle: clone 给 TunnelRegistry，供 visitor handler 调用 open_bidirectional_stream()
let handle_for_visitor_1 = handle.clone();
let handle_for_visitor_2 = handle.clone();

// StreamAcceptor: 可丢弃（tunnel 模式下 server 不再需要 accept client 发起的 stream）
// 或保留用于检测连接断开（acceptor 返回 None = 连接关闭）
tokio::spawn(async move {
    // 等待连接关闭
    while acceptor.accept().await.is_ok_and(|v| v.is_some()) {}
    // 触发该 client 路由清理
});
```

**Client 侧架构**：

```rust
// auth 阶段：在完整 Connection 上做 auth stream 握手
let auth_stream = connection.open_bidirectional_stream().await?;
// ... 发送 FLAG_AUTH，读取 FLAG_AUTH_ACK ...

// auth 完成后 split
let (_handle, mut acceptor) = connection.split();

// StreamAcceptor: 接收 server 发起的反向 stream
while let Some(stream) = acceptor.accept_bidirectional_stream().await? {
    // 处理 FLAG_REVERSE_OPEN ...
}
// acceptor 返回 None = 连接关闭，触发重连
```

> **为什么不需要 `Arc<Mutex<>>`**：`Handle` 实现了 `Clone`，每个 visitor handler task 持有独立的 `Handle` clone，各自调用 `open_bidirectional_stream(&mut self)` 时是对各自 clone 的独占访问，无需互斥。s2n-quic 内部保证多个 Handle clone 之间打开 stream 的 stream ID 分配是安全的。

> **断线重连**：client 侧 `acceptor.accept()` 返回 `None` 时表示连接关闭，可安全地用新 `Connection` 重新走 auth + split 流程。

**`MuxConnection` trait 需新增 `accept_stream()` 方法：**

现有 `MuxConnection` trait（`tunnel/client.rs`）仅定义了 `open_stream()`，`accept_stream()` 方法被注释掉了。tunnel 模式需要启用该方法：

```rust
pub(crate) trait MuxConnection {
    type SendStream: AsyncWrite + Unpin + Send;
    type RecvStream: AsyncRead + Unpin + Send;
    async fn ping(&mut self) -> anyhow::Result<()>;
    async fn connect(&mut self, url: &Url, key_path: &Path, host: &str) -> anyhow::Result<()>;
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;
    async fn accept_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)>;  // 新增
    fn is_valid(&self) -> bool;
    fn set_connection(&mut self, new_c: Self);
}
```

实现说明：
- **TLS 模式**：`TlsConnection::accept_stream()` 代理到内部 `mux::Connection::accept_stream()` 获取完整 `MuxStream`，再通过 `tokio::io::split()` 拆分为 `(WriteHalf<MuxStream>, ReadHalf<MuxStream>)`，与现有 `open_stream()` 实现模式一致（`tls_client.rs:62-64`）
- **QUIC 模式**：`S2NQuicConnection::accept_stream()` 在 auth 完成后通过 `connection.split()` 得到 `StreamAcceptor`，调用 `acceptor.accept_bidirectional_stream()` 获取 `BidirectionalStream`，再调用 `.split()` 拆分为 `(SendStream, ReceiveStream)`，与现有 `open_stream()` 实现模式一致（`s2n_quic_client.rs:63-65`）

**新增 flag 在 mux connection dispatch 中的路径：**

`FLAG_AUTH`、`FLAG_AUTH_ACK`、`FLAG_REVERSE_OPEN` 均为 **stream 内部首帧 flag**（即它们出现在某条 stream 的第一个 event 中），而非 mux connection 层面的控制帧。

**TLS mux 模式的嵌套传输机制**：上层通过 `event::write_event(&mut mux_stream, ev)` 将 event（如 `FLAG_AUTH`）写入 MuxStream，MuxStream 的 AsyncWrite 将 raw bytes 封装为 `Control::StreamData`，connection 层将其作为 `FLAG_DATA` event 发到 wire。接收端 connection 层只看到 `FLAG_DATA`，将 body 路由到对应 stream 的 channel；上层从 MuxStream 读取 raw bytes 后调用 `event::read_event()` 解码出原始 flag（`FLAG_AUTH` 等）。因此 `connection.rs` 的 match 语句无需为这些 flag 添加新分支。

**QUIC 模式**：无 mux 层，events 直接通过 QUIC bidirectional stream 读写，flag 值直接出现在 wire 上。

### 与现有代码的集成点

| 模块 | 变更 |
|------|------|
| `main.rs` | 新增 `--tunnel`、`--tunnel-port-range`、`--tunnel-client-id` 参数；`--tunnel` 添加 `conflicts_with = ["listen", "tproxy"]`（clap 参数名）；tunnel 模式下走不同的 client 逻辑分支；server 端解析 port-range 并构建 `TunnelManager` 传入 remote server |
| `tunnel/mod.rs` | 新增 `tunnel_client.rs`、`tunnel_remote.rs` 模块 |
| `tunnel/client.rs` | proxy 模式 `mux_client_loop` 和 `Message` 不用于 tunnel；tunnel 模式有独立的 client 流程：auth stream 握手 + accept 循环接收反向 stream |
| `tunnel/tls_client.rs` | tunnel 模式需独立的 client loop：auth stream 发送 `FLAG_AUTH` + `AuthRequest::Register`，读取 `FLAG_AUTH_ACK` + `AuthAck::RegisterAck`，同时 accept_stream 处理 FLAG_REVERSE_OPEN |
| `tunnel/tls_remote.rs` | 连接处理流程变更：`accept_stream()` 得到 auth stream → 读取首帧 `FLAG_AUTH` → 解码 `AuthRequest` → `AuthRequest::Proxy` → proxy 模式（回复 `AuthAck::Proxy`，auth stream 关闭，与现有实现一致），`AuthRequest::Register` → tunnel 模式（校验 → 回复 `AuthAck::RegisterAck` → auth stream 关闭 → 保存 mux::Connection 到 TunnelRegistry） |
| `tunnel/s2n_quic_client.rs` | tunnel 模式需独立的 client loop：auth stream 发送 `FLAG_AUTH` + `AuthRequest::Register`，读取 `FLAG_AUTH_ACK` + `AuthAck::RegisterAck`，auth 完成后调用 `connection.split()` 得到 `(Handle, StreamAcceptor)`，进入 `StreamAcceptor::accept_bidirectional_stream()` 循环接收 server 发起的反向 stream；`S2NQuicConnection` 需实现 `MuxConnection::accept_stream()` |
| `tunnel/s2n_quic_remote.rs` | tunnel 模式下 auth 完成后调用 `connection.split()` 得到 `(Handle, StreamAcceptor)`，将 `Handle`（实现 Clone）存入 TunnelRegistry 供 visitor handler 调用 `open_bidirectional_stream()`；`StreamAcceptor` 用于检测连接断开 |
| `tunnel/tls_local.rs` | 复用 `peek_sni_v2(&TcpStream)` 逻辑，供 server 端 SNI 路由使用 |
| `mux/event.rs` | 新增 `FLAG_AUTH=6`/`FLAG_AUTH_ACK=9`/`FLAG_REVERSE_OPEN=10` 常量；新增 `AuthRequest`/`AuthAck`/`RegisterRequest`/`RegisterAck`/`TunnelResult`/`TunnelEntry` payload 结构；新增 `new_auth_event()`/`new_auth_ack_event()`/`new_reverse_open_stream_event()` factory 函数；`new_reverse_open_stream_event()` 与 `new_open_stream_event()` 逻辑相同但使用 `FLAG_REVERSE_OPEN` |
| `tunnel/local.rs` | tunnel 模式下不启动 `start_local_tunnel_server`（由 `conflicts_with` 保证） |

## 安全性

### 认证

- **第一版不实现 mTLS**，沿用现有单向认证（client 验证 server 证书）
- 安全性依赖：`--tunnel-port-range` 约束可用端口范围 + 部署时的网络隔离
- mTLS（server 验证 client 证书）作为后续增强，不阻塞 NAT traversal 功能开发

### 端口安全

- `--tunnel-port-range` 白名单机制，禁止特权端口
- Server 拒绝自环端口注册
- 未配置 port-range 则 tunnel 功能完全不启用

## 不在范围内（后续版本）

- mTLS 双向认证
- UDP 隧道（当前仅 TCP）
- 访问控制：限制公网端口的来源 IP
- 流量限速：per-tunnel 或 per-client 的带宽限制
- Token 认证：`AuthRequest` 消息中携带 pre-shared token
- 多级中继（client → relay → server）
- Web UI 管理面板
- 自动 HTTPS 证书（Let's Encrypt）
