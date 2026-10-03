# Tunnel Mode Multi-Connection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Support multiple TLS/QUIC connections in tunnel mode by spawning `--concurrent` independent tunnel client tasks.

**Architecture:** Spawn N independent tunnel client tasks (one per `--concurrent`), each with its own connection, auth, and reconnection loop. Server-side already supports multi-connection per client_id via `ClientState.connections` Vec with round-robin `next_connection()`.

**Tech Stack:** Rust, tokio, existing mux/tunnel infrastructure

---

## File Structure

| File | Change |
|------|--------|
| `src/main.rs:285` | Pass `args.concurrent` to `start_tunnel_client` |
| `src/tunnel/mod.rs:37-62` | Add `concurrent: usize` param to `start_tunnel_client`, pass through |
| `src/tunnel/tunnel_client.rs:17-72` | Add `concurrent` param, spawn N tasks |
| `src/tunnel/s2n_quic_client.rs:178-231` | Add `concurrent` param, spawn N tasks |

No server-side changes needed — `TunnelRegistry` already handles multiple connections per client.

---

### Task 1: Thread `concurrent` param through `start_tunnel_client`

**Files:**
- Modify: `src/main.rs:285-293`
- Modify: `src/tunnel/mod.rs:37-62`

- [ ] **Step 1: Update `start_tunnel_client` signature in `mod.rs`**

Add `concurrent: usize` parameter and pass it to both TLS and QUIC branches:

```rust
pub async fn start_tunnel_client(
    url: &url::Url,
    cert_path: &std::path::Path,
    host: &str,
    idle_timeout_secs: usize,
    stream_window: u32,
    concurrent: usize,
    app_config: std::sync::Arc<crate::app_config::AppConfig>,
) -> anyhow::Result<()> {
    match url.scheme() {
        "tls" => {
            tunnel_client::start_tunnel_client_tls(
                url,
                cert_path,
                host,
                idle_timeout_secs,
                stream_window,
                concurrent,
                app_config,
            )
            .await
        }
        "quic" => {
            start_tunnel_client_quic(url, cert_path, host, app_config, idle_timeout_secs, concurrent).await
        }
        _ => Err(anyhow::anyhow!("unsupported scheme: {}", url.scheme())),
    }
}
```

- [ ] **Step 2: Update call site in `main.rs`**

At `src/main.rs:285`, add `args.concurrent` to the call:

```rust
tunnel::start_tunnel_client(
    args.remote.as_ref().unwrap(),
    &args.cert,
    &args.tls_host,
    args.idle_timeout_secs,
    args.mux_stream_window,
    args.concurrent,
    app_config,
)
.await?;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: compile errors in `start_tunnel_client_tls` and `start_tunnel_client_quic` (signature mismatch — expected, fixed in next tasks)

- [ ] **Step 4: Commit**

```bash
git add src/main.rs src/tunnel/mod.rs
git commit -m "refactor: thread concurrent param to tunnel client start functions"
```

---

### Task 2: Multi-connection TLS tunnel client

**Files:**
- Modify: `src/tunnel/tunnel_client.rs:17-72`

- [ ] **Step 1: Update `start_tunnel_client_tls` to spawn N tasks**

Replace the current function with one that spawns `concurrent` independent tasks. Each task runs the existing reconnection loop:

```rust
pub async fn start_tunnel_client_tls(
    url: &Url,
    cert_path: &Path,
    host: &str,
    idle_timeout_secs: usize,
    stream_window: u32,
    concurrent: usize,
    app_config: Arc<AppConfig>,
) -> Result<()> {
    let mut handles = Vec::with_capacity(concurrent);
    for i in 0..concurrent {
        let url = url.clone();
        let cert_path = cert_path.to_path_buf();
        let host = host.to_string();
        let app_config = app_config.clone();
        handles.push(tokio::spawn(async move {
            tunnel_client_loop_tls(
                &url,
                &cert_path,
                &host,
                idle_timeout_secs,
                stream_window,
                i,
                app_config,
            )
            .await;
        }));
    }
    // Wait for all tasks (they run forever due to reconnection loops)
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}
```

- [ ] **Step 2: Extract the reconnection loop into `tunnel_client_loop_tls`**

Move the current body of `start_tunnel_client_tls` into a private function. The only change is adding a `conn_index` parameter for logging:

```rust
async fn tunnel_client_loop_tls(
    url: &Url,
    cert_path: &Path,
    host: &str,
    idle_timeout_secs: usize,
    stream_window: u32,
    conn_index: usize,
    app_config: Arc<AppConfig>,
) {
    const INITIAL_BACKOFF_SECS: u64 = 1;
    const MAX_BACKOFF_SECS: u64 = 60;

    let mut backoff_secs = INITIAL_BACKOFF_SECS;
    loop {
        let (client_id, entries) = {
            let cfg = app_config.reloadable.lock().await;
            (cfg.tunnel_client_id.clone(), cfg.tunnel_entries.clone())
        };

        let start = Instant::now();
        let token = app_config.reload_token_clone().await;

        let result = tokio::select! {
            r = run_tunnel_connection_tls(
                url,
                cert_path,
                host,
                stream_window,
                &client_id,
                &entries,
                idle_timeout_secs,
            ) => r,
            _ = token.cancelled() => {
                tracing::info!("[conn-{}] Config reloaded, reconnecting TLS tunnel with new entries...", conn_index);
                backoff_secs = INITIAL_BACKOFF_SECS;
                continue;
            }
        };

        if start.elapsed() > Duration::from_secs(30) {
            backoff_secs = INITIAL_BACKOFF_SECS;
        }

        tracing::info!(
            "[conn-{}] Tunnel connection lost ({}), reconnecting in {}s...",
            conn_index,
            result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            backoff_secs
        );
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: success

- [ ] **Step 4: Commit**

```bash
git add src/tunnel/tunnel_client.rs
git commit -m "feat(tunnel): support multiple TLS connections via --concurrent"
```

---

### Task 3: Multi-connection QUIC tunnel client

**Files:**
- Modify: `src/tunnel/s2n_quic_client.rs:178-231`

- [ ] **Step 1: Update `start_tunnel_client_quic` to spawn N tasks**

Same pattern as TLS. Replace the current function:

```rust
pub async fn start_tunnel_client_quic(
    url: &Url,
    cert_path: &Path,
    host: &str,
    app_config: Arc<crate::app_config::AppConfig>,
    idle_timeout_secs: usize,
    concurrent: usize,
) -> anyhow::Result<()> {
    let mut handles = Vec::with_capacity(concurrent);
    for i in 0..concurrent {
        let url = url.clone();
        let cert_path = cert_path.to_path_buf();
        let host = host.to_string();
        let app_config = app_config.clone();
        handles.push(tokio::spawn(async move {
            tunnel_client_loop_quic(
                &url,
                &cert_path,
                &host,
                app_config,
                idle_timeout_secs,
                i,
            )
            .await;
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}
```

- [ ] **Step 2: Extract the reconnection loop into `tunnel_client_loop_quic`**

Move the current body into a private function with `conn_index`:

```rust
async fn tunnel_client_loop_quic(
    url: &Url,
    cert_path: &Path,
    host: &str,
    app_config: Arc<crate::app_config::AppConfig>,
    idle_timeout_secs: usize,
    conn_index: usize,
) {
    const INITIAL_BACKOFF_SECS: u64 = 1;
    const MAX_BACKOFF_SECS: u64 = 60;

    let mut backoff_secs = INITIAL_BACKOFF_SECS;
    loop {
        let (client_id, entries) = {
            let cfg = app_config.reloadable.lock().await;
            (cfg.tunnel_client_id.clone(), cfg.tunnel_entries.clone())
        };

        let start = std::time::Instant::now();
        let token = app_config.reload_token_clone().await;

        let result = tokio::select! {
            r = run_quic_tunnel_connection(
                url,
                cert_path,
                host,
                &client_id,
                &entries,
                idle_timeout_secs,
            ) => r,
            _ = token.cancelled() => {
                tracing::info!("[conn-{}] Config reloaded, reconnecting QUIC tunnel with new entries...", conn_index);
                backoff_secs = INITIAL_BACKOFF_SECS;
                continue;
            }
        };

        if start.elapsed() > Duration::from_secs(30) {
            backoff_secs = INITIAL_BACKOFF_SECS;
        }

        tracing::info!(
            "[conn-{}] QUIC tunnel connection lost ({}), reconnecting in {}s...",
            conn_index,
            result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            backoff_secs
        );
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check --features s2n_quic`
Expected: success

- [ ] **Step 4: Commit**

```bash
git add src/tunnel/s2n_quic_client.rs
git commit -m "feat(tunnel): support multiple QUIC connections via --concurrent"
```

---

### Task 4: Final verification

- [ ] **Step 1: Run full build**

Run: `cargo build`
Expected: success

- [ ] **Step 2: Run tests**

Run: `cargo test`
Expected: all tests pass

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --all-features 2>&1 | head -30`
Expected: no new warnings

- [ ] **Step 4: Commit any fixes**

If clippy or tests found issues, fix and commit.
