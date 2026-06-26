//! End-to-end relay throughput benchmark.
//!
//! Spins up two `Connection`s over a `tokio::io::duplex` pair, opens a
//! stream, transfers `TOTAL_BYTES` from client → server, and has the
//! server echo them back. Measures wall-clock time and computed
//! throughput in MiB/s.
//!
//! Usage:
//!     cargo run --release --example bench_relay
//!     TOTAL_BYTES=16777216 cargo run --release --example bench_relay
//!
//! Run once against the baseline (git stash the src/ fixes) and once
//! against the fixed tree to compare.

use rsnova::mux::Connection;
use rsnova::mux::Mode;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DEFAULT_TOTAL_BYTES: usize = 4 * 1024 * 1024; // 4 MiB
const CHUNK: usize = 32 * 1024; // 32 KiB per write — matches the H2 buffer

fn main() -> std::io::Result<()> {
    let total_bytes = std::env::var("TOTAL_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_TOTAL_BYTES);
    let warmup = std::env::var("WARMUP_ITERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);
    let measured = std::env::var("MEASURED_ITERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(3);

    println!(
        "bench_relay: {} bytes total per iter, chunk={}, warmup={}, measured={}",
        total_bytes,
        CHUNK,
        warmup,
        measured
    );

    // Single runtime for the whole process. We don't try to cleanly shut
    // down dispatcher tasks between iterations — the process exits when
    // done. This avoids the cost (and hang risk) of dropping spawned
    // dispatcher tasks that are still waiting on the underlying duplex.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let mut warmup_times = Vec::with_capacity(warmup);
    let mut measured_times = Vec::with_capacity(measured);

    for i in 0..(warmup + measured) {
        let elapsed = rt
            .block_on(run_once(total_bytes))
            .expect("iteration failed");
        if i < warmup {
            warmup_times.push(elapsed);
        } else {
            measured_times.push(elapsed);
        }
    }

    // Report.
    println!("\n--- Warmup (excluded from stats) ---");
    for (i, t) in warmup_times.iter().enumerate() {
        let secs = t.as_secs_f64();
        let mibps = (total_bytes as f64) / secs / (1024.0 * 1024.0);
        println!("  warmup[{}]: {:.3}ms  {:.2} MiB/s", i, secs * 1e3, mibps);
    }
    println!("\n--- Measured ---");
    for (i, t) in measured_times.iter().enumerate() {
        let secs = t.as_secs_f64();
        let mibps = (total_bytes as f64) / secs / (1024.0 * 1024.0);
        println!("  run[{}]: {:.3}ms  {:.2} MiB/s", i, secs * 1e3, mibps);
    }

    let mean_ms = measured_times.iter().map(|t| t.as_secs_f64() * 1e3).sum::<f64>()
        / measured_times.len() as f64;
    let mean_mibps = (total_bytes as f64)
        / (mean_ms / 1e3)
        / (1024.0 * 1024.0);
    let min_ms = measured_times
        .iter()
        .map(|t| t.as_secs_f64() * 1e3)
        .fold(f64::INFINITY, f64::min);
    let max_ms = measured_times
        .iter()
        .map(|t| t.as_secs_f64() * 1e3)
        .fold(0.0, f64::max);
    println!(
        "\nmean: {:.3}ms  {:.2} MiB/s   (min {:.3}ms, max {:.3}ms, n={})",
        mean_ms, mean_mibps, min_ms, max_ms, measured_times.len()
    );

    Ok(())
}

async fn run_once(total_bytes: usize) -> std::io::Result<std::time::Duration> {
    // 8 MiB duplex — large enough that the buffer doesn't bottleneck
    // the relay. The mux layer has its own flow control windows.
    let (a, b) = tokio::io::duplex(8 * 1024 * 1024);
    let (a_r, a_w) = tokio::io::split(a);
    let (b_r, b_w) = tokio::io::split(b);
    let client = Connection::new_with_stream_window(a_r, a_w, Mode::Client, 0, 256 * 1024);
    let server = Connection::new_with_stream_window(b_r, b_w, Mode::Server, 1, 256 * 1024);

    let client_stream = client.open_stream().await.expect("open_stream");
    let server_stream = server.accept_stream().await.expect("accept_stream");
    let (mut cr, mut cw) = tokio::io::split(client_stream);
    let (mut sr, mut sw) = tokio::io::split(server_stream);

    // Echo: server reads from sr and writes back to sw. Runs concurrently
    // with the client writer below; if we serialized (write all then read)
    // we'd deadlock against the mux flow-control window (256KB) once
    // total_bytes exceeds it.
    let echo = tokio::spawn(async move {
        let mut buf = vec![0u8; CHUNK];
        let mut remaining = total_bytes;
        while remaining > 0 {
            let n = match sr.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    eprintln!("echo read err: {}", e);
                    return;
                }
            };
            if let Err(e) = sw.write_all(&buf[..n]).await {
                eprintln!("echo write err: {}", e);
                return;
            }
            remaining -= n;
        }
        let _ = sw.shutdown().await;
    });

    // Writer: client writes payload in CHUNK-sized chunks.
    let chunk = vec![0xABu8; CHUNK];
    let start = Instant::now();

    // Run writer and reader concurrently so the client can drain the echo
    // (and send window updates back to the server) while still writing.
    let write_total = total_bytes;
    let writer = async move {
        let mut written = 0usize;
        while written < write_total {
            let n = (write_total - written).min(CHUNK);
            cw.write_all(&chunk[..n]).await.expect("client write");
            written += n;
        }
        let _ = cw.shutdown().await;
    };

    let mut read = 0usize;
    let mut buf = vec![0u8; CHUNK];
    let reader = async {
        while read < total_bytes {
            let n = match cr.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    eprintln!("client read err: {}", e);
                    break;
                }
            };
            read += n;
        }
    };

    let (_, _) = tokio::join!(writer, reader);
    let elapsed = start.elapsed();

    echo.await.expect("echo task");

    assert_eq!(read, total_bytes, "client did not receive all echoed bytes");
    Ok(elapsed)
}
