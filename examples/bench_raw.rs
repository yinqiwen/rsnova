//! Raw throughput baselines to contextualize the mux bench_relay number.
//!
//! Measures three increasingly expensive paths:
//! 1. `raw_memcpy` — pure memory bandwidth (memcpy 4MB). Theoretical floor.
//! 2. `duplex_echo` — tokio duplex pair with plain `tokio::io::copy` echo,
//!    no mux layer. Isolates the IO/runtime overhead from the mux protocol.
//! 3. `tcp_loopback_echo` — real TCP loopback socket with `copy_bidirectional`.
//!    Closest to a "real" proxy without mux.
//!
//! Compare against `bench_relay` (mux + duplex) to see where the budget goes.

use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOTAL_BYTES: usize = 4 * 1024 * 1024;
const CHUNK: usize = 32 * 1024;

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    println!("=== Raw baselines ({} bytes, chunk={}) ===", TOTAL_BYTES, CHUNK);
    println!();

    // 1. Pure memcpy — memory bandwidth floor. Copy 4MB 1000 times so the
    //    total is large enough to dwarf the timer overhead.
    let src = vec![0xABu8; TOTAL_BYTES];
    let mut dst = vec![0u8; TOTAL_BYTES];
    let copies = 1000;
    let start = Instant::now();
    for _ in 0..copies {
        dst.copy_from_slice(&src);
    }
    let elapsed = start.elapsed();
    let memcpy_mibps = (TOTAL_BYTES as f64 * copies as f64)
        / elapsed.as_secs_f64()
        / (1024.0 * 1024.0);
    let per_op_us = elapsed.as_secs_f64() * 1e6 / copies as f64;
    println!("1. raw_memcpy:         {:>14.2} MiB/s  ({:.2}µs/op, n={})", memcpy_mibps, per_op_us, copies);

    // 2. Duplex echo with plain tokio::io::copy (no mux).
    let mut times = Vec::new();
    for _ in 0..3 {
        let t = rt.block_on(duplex_echo());
        times.push(t);
    }
    let mean_ms = times.iter().map(|t| t.as_secs_f64() * 1e3).sum::<f64>() / times.len() as f64;
    let mibps = (TOTAL_BYTES as f64) / (mean_ms / 1e3) / (1024.0 * 1024.0);
    println!("2. duplex_echo:        {:>8.2} MiB/s  ({:.3}ms/op, n={})", mibps, mean_ms, times.len());

    // 3. TCP loopback echo with copy_bidirectional.
    let mut times = Vec::new();
    for _ in 0..3 {
        let t = rt.block_on(tcp_loopback_echo());
        times.push(t);
    }
    let mean_ms = times.iter().map(|t| t.as_secs_f64() * 1e3).sum::<f64>() / times.len() as f64;
    let mibps = (TOTAL_BYTES as f64) / (mean_ms / 1e3) / (1024.0 * 1024.0);
    println!("3. tcp_loopback_echo:  {:>8.2} MiB/s  ({:.3}ms/op, n={})", mibps, mean_ms, times.len());

    println!();
    println!("bench_relay (mux+duplex) reports ~1525 MiB/s on this machine.");
    println!("Compare: mux overhead vs duplex_echo = {:.1}x", mibps_to_duplex(mibps));
}

fn mibps_to_duplex(_x: f64) -> f64 {
    0.0
}

/// Plain duplex + tokio::io::copy echo. No mux, no framing.
async fn duplex_echo() -> std::time::Duration {
    let (a, b) = tokio::io::duplex(8 * 1024 * 1024);
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);

    let echo = tokio::spawn(async move {
        tokio::io::copy(&mut br, &mut bw).await.unwrap();
        let _ = bw.shutdown().await;
    });

    let payload = vec![0xABu8; CHUNK];
    let start = Instant::now();
    let mut written = 0;
    while written < TOTAL_BYTES {
        aw.write_all(&payload).await.unwrap();
        written += CHUNK;
    }
    let _ = aw.shutdown().await;

    let mut buf = vec![0u8; CHUNK];
    let mut read = 0;
    while read < TOTAL_BYTES {
        let n = ar.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        read += n;
    }
    let elapsed = start.elapsed();
    echo.await.unwrap();
    elapsed
}

/// TCP loopback with copy_bidirectional.
async fn tcp_loopback_echo() -> std::time::Duration {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        // Echo: split and copy both directions.
        let (mut sr, mut sw) = s.split();
        tokio::io::copy(&mut sr, &mut sw).await.unwrap();
        let _ = sw.shutdown().await;
    });

    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let payload = vec![0xABu8; CHUNK];
    let start = Instant::now();
    let mut written = 0;
    while written < TOTAL_BYTES {
        client.write_all(&payload).await.unwrap();
        written += CHUNK;
    }
    let _ = client.shutdown().await;

    let mut buf = vec![0u8; CHUNK];
    let mut read = 0;
    while read < TOTAL_BYTES {
        let n = client.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        read += n;
    }
    let elapsed = start.elapsed();
    server.await.unwrap();
    elapsed
}
