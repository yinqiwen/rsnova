# rsnova

Rust 实现的安全代理/隧道工具，基于 QUIC 和 TLS 协议提供多路复用的网络隧道。

## 特性

- **加密传输** — 支持 TLS (rustls) 和 QUIC (s2n-quic) 两种加密隧道协议，服务端同时监听
- **多协议代理** — 自动识别 SOCKS5、HTTP/HTTPS、TLS SNI 协议，无法识别时回退到透明代理
- **流多路复用** — 单条连接上承载多个数据流，内置背压控制和连接池
- **NAT 穿透** — 反向隧道模式，将内网服务暴露到公网，支持 SNI 域名路由
- **透明代理** — Linux 下支持 `SO_ORIGINAL_DST` / tproxy（TCP + UDP）
- **自签证书** — 内置 `--rcgen` 一键生成 TLS 证书
- **后台运行** — Unix 下支持 `-d` 守护进程模式
- **日志轮转** — `--log` 指定日志文件，自动按天轮转
- **监控接口** — 内置 Admin HTTP 服务，提供 `/metrics` 端点

## 使用场景

| 场景 | 说明 |
|------|------|
| 安全代理 | 客户端提供 SOCKS5/HTTP 代理入口，通过加密隧道转发到服务端访问目标网络 |
| 内网穿透 | 将内网服务（SSH、数据库、Web 等）通过反向隧道暴露到公网服务器 |
| 透明代理 | Linux 网关上配合 iptables/nftables 做透明代理，无需客户端配置 |
| TLS SNI 路由 | 根据 TLS ClientHello 中的 SNI 域名路由到不同后端服务 |

## 使用说明

### 构建

```sh
cargo build --release
```

### 1. 生成证书

```sh
./target/release/rsnova --rcgen true --tls_host mydomain.io
```

生成 `cert.pem` 和 `key.pem`。

### 2. 启动服务端

服务端同时监听 TLS (TCP) 和 QUIC (UDP)，无需指定协议：

```sh
./target/release/rsnova --role server --key key.pem --cert cert.pem --listen 0.0.0.0:48100
```

### 3. 启动客户端

客户端根据 `--remote` 的 URL scheme 自动选择协议：

```sh
# TLS
./target/release/rsnova --role client --cert cert.pem --listen 127.0.0.1:48100 --remote tls://<server-ip>:48100 --tls_host mydomain.io

# QUIC
./target/release/rsnova --role client --cert cert.pem --listen 127.0.0.1:48100 --remote quic://<server-ip>:48100 --tls_host mydomain.io
```

### 4. 使用代理

浏览器或工具配置代理为 `socks5://127.0.0.1:48100` 或 `http://127.0.0.1:48100`。

### NAT 穿透（反向隧道）

```sh
# 客户端：将本地 SSH (22) 映射到服务端 2222 端口
./target/release/rsnova --role client --cert cert.pem --remote tls://<server-ip>:48100 \
  --tls_host mydomain.io --tunnel-client-id myhost \
  --tunnel 22:2222

# 服务端：允许隧道使用 8000-9000 端口
./target/release/rsnova --role server --key key.pem --cert cert.pem \
  --listen 0.0.0.0:48100 --tunnel-port-range 8000-9000
```

隧道格式：

| 格式 | 说明 |
|------|------|
| `port` | localhost:port → 远端同端口 |
| `localPort:remotePort` | localhost:localPort → 远端 remotePort |
| `host:localPort:remotePort` | host:localPort → 远端 remotePort |
| `host:localPort:remotePort:sni` | 同上，附带 SNI 域名路由 |
| `[ipv6]:localPort:remotePort[:sni]` | IPv6 地址支持 |

### 配置文件

支持 TOML 配置文件（`-c config.toml`），CLI 参数优先级高于配置文件：

```toml
listen = "127.0.0.1:48100"
role = "client"
remote = "tls://1.2.3.4:48100"
cert = "cert.pem"
key = "key.pem"
tls_host = "mydomain.io"
concurrent = 5
threads = 2
idle_timeout_secs = 30
max_connections = 256
admin_listen = "127.0.0.1:48102"
```

### 常用参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--listen` | `127.0.0.1:48100` | 代理监听地址 |
| `--role` | `client` | `client` 或 `server` |
| `--remote` | — | 远端地址 (`tls://host:port` 或 `quic://host:port`) |
| `--concurrent` | `5` | 连接池大小 |
| `--max-connections` | `256` | 最大并发代理连接数 |
| `--tunnel` | — | 隧道条目（与 `--listen`/`--tproxy` 互斥） |
| `--tunnel-port-range` | — | 服务端允许的端口范围，如 `8000-9000,10000-10100` |
| `--tproxy` | `false` | 透明代理模式 (Linux) |
| `-d, --daemon` | `false` | 后台运行 (Unix) |
| `--log` | — | 日志文件路径 |
| `-c, --config` | — | TOML 配置文件路径 |
