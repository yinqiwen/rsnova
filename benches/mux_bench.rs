//! Microbenchmarks for the mux layer hot paths addressed by the
//! performance pass:
//!
//! - `mux_stream_poll_write_8kb` — exercises H1 (MuxStream::poll_write
//!   Bytes::copy_from_slice → BytesMut::with_capacity+extend_from_slice).
//! - `mux_stream_poll_write_32kb` — same path with a 32KB buffer (closer
//!   to the H2 relay buffer size after the change).
//! - `event_write_data_event` — measures `event::write_event` throughput
//!   for DATA frames; touches M1/M2 paths used on the write side.
//! - `event_read_data_event` — measures `event::read_event` for DATA
//!   frames; touches M1 (BytesMut::zeroed → with_capacity+read_buf).
//! - `connection_duplex_64kb` — end-to-end micro: client writes 64KB
//!   through a stream, server echoes back. Exercises H4 (dispatcher
//!   metrics sampling), H5 (window_update drain), and H1+M1 together.
//! - `window_update_event_roundtrip` — write+read a WINDOW_UPDATE frame;
//!   covers M2 + the small-body allocation path.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use rsnova::mux::Connection;
use rsnova::mux::event;
use rsnova::mux::Mode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Spawn a runtime once per criterion process. Criterion's async_tokio
/// feature provides a runtime per iteration but creating it per-bench is
/// also fine; we use the `async_tokio` async runner.
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build runtime")
}

/// H1: MuxStream::poll_write on an 8KB buffer.
///
/// We can't easily isolate `poll_write` without the surrounding
/// dispatcher, so we measure end-to-end `write_all` which calls
/// `poll_write` once per buffer. The dispatcher task is wired up by
/// `Connection::new_with_stream_window` over a duplex pair.
fn bench_mux_stream_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("mux_stream_write");
    for size in [8 * 1024usize, 32 * 1024] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.to_async(&rt()).iter(|| {
                async move {
                    let (a, b) = tokio::io::duplex(8 * 1024 * 1024);
                    let (a_r, a_w) = tokio::io::split(a);
                    let (b_r, b_w) = tokio::io::split(b);
                    let client = Connection::new_with_stream_window(
                        a_r,
                        a_w,
                        Mode::Client,
                        0,
                        256 * 1024,
                    );
                    let server = Connection::new_with_stream_window(
                        b_r,
                        b_w,
                        Mode::Server,
                        1,
                        256 * 1024,
                    );

                    let client_stream = client.open_stream().await.expect("open_stream");
                    let server_stream = server.accept_stream().await.expect("accept_stream");
                    let (_cr, mut cw) = tokio::io::split(client_stream);
                    let (mut sr, mut sw) = tokio::io::split(server_stream);

                    let payload = vec![0xABu8; size];
                    let echo = tokio::spawn(async move {
                        let mut buf = vec![0u8; size];
                        let _ = sr.read_exact(&mut buf).await;
                        let _ = sw.write_all(&buf).await;
                        let _ = sw.shutdown().await;
                    });

                    let _ = cw.write_all(&payload).await;
                    let _ = cw.shutdown().await;
                    let _ = echo.await;
                    // Drop both connections to clean up dispatcher tasks.
                    client.close();
                    server.close();
                    black_box(payload.len());
                }
            });
        });
    }
    group.finish();
}

/// M1 + write_event path. Writes a DATA frame to a Vec-backed sink
/// (no dispatcher) so this isolates `event::write_event` cost.
fn bench_event_write_data(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_write_data");
    for size in [4usize, 8 * 1024, 32 * 1024, 256 * 1024] {
        group.throughput(Throughput::Bytes((size + 8) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                let body = bytes::Bytes::from(vec![0xCDu8; size]);
                let ev = event::new_data_event(42, body);
                let mut sink: Vec<u8> = Vec::with_capacity(size + 8);
                // write_event is async but the Vec sink completes
                // synchronously; use a tiny runtime to drive it.
                let runtime = rt();
                runtime.block_on(event::write_event(&mut sink, ev)).unwrap();
                black_box(sink.len());
            });
        });
    }
    group.finish();
}

/// M1: read_event. Reads back a DATA frame previously serialized.
fn bench_event_read_data(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_read_data");
    for size in [4usize, 8 * 1024, 32 * 1024, 256 * 1024] {
        group.throughput(Throughput::Bytes((size + 8) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            // Pre-serialize a DATA frame once per input.
            let body = bytes::Bytes::from(vec![0xCDu8; size]);
            let ev = event::new_data_event(42, body);
            let mut wire: Vec<u8> = Vec::with_capacity(size + 8);
            let runtime = rt();
            runtime.block_on(event::write_event(&mut wire, ev)).unwrap();
            b.iter(|| {
                let mut reader = &wire[..];
                let runtime = rt();
                // read_event is async; use a tiny runtime to drive it. The
                // slice reader completes in one poll so runtime overhead is
                // a fixed constant per iteration.
                let ev = runtime.block_on(event::read_event(&mut reader)).unwrap();
                black_box(ev.body.len());
            });
        });
    }
    group.finish();
}

/// M2: WINDOW_UPDATE frame write+read roundtrip.
fn bench_window_update_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("window_update_roundtrip");
    group.throughput(Throughput::Elements(1));
    group.bench_function("write_read", |b| {
        b.iter(|| {
            let ev = event::new_window_update_event(42, 131072);
            let mut sink: Vec<u8> = Vec::with_capacity(16);
            let runtime = rt();
            runtime.block_on(event::write_event(&mut sink, ev)).unwrap();
            let mut reader = &sink[..];
            let _ev = runtime.block_on(event::read_event(&mut reader)).unwrap();
        });
    });
    group.finish();
}

/// H4 + H5 + H1 + M1 end-to-end: client writes 64KB through a stream,
/// server echoes. Exercises the full dispatcher loop including the
/// per-frame window_update drain (H5) and metrics sampling (H4).
fn bench_connection_duplex(c: &mut Criterion) {
    let mut group = c.benchmark_group("connection_duplex");
    for size in [4 * 1024usize, 64 * 1024, 256 * 1024] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.to_async(&rt()).iter(|| {
                async move {
                    let (a, b) = tokio::io::duplex(8 * 1024 * 1024);
                    let (a_r, a_w) = tokio::io::split(a);
                    let (b_r, b_w) = tokio::io::split(b);
                    let client = Connection::new_with_stream_window(
                        a_r,
                        a_w,
                        Mode::Client,
                        0,
                        256 * 1024,
                    );
                    let server = Connection::new_with_stream_window(
                        b_r,
                        b_w,
                        Mode::Server,
                        1,
                        256 * 1024,
                    );

                    let client_stream = client.open_stream().await.expect("open_stream");
                    let server_stream = server.accept_stream().await.expect("accept_stream");
                    let (mut cr, mut cw) = tokio::io::split(client_stream);
                    let (mut sr, mut sw) = tokio::io::split(server_stream);

                    let payload = vec![0xABu8; size];
                    let echo_payload = payload.clone();
                    let echo = tokio::spawn(async move {
                        let mut buf = vec![0u8; size.min(32 * 1024)];
                        let mut remaining = size;
                        while remaining > 0 {
                            let n = sr.read(&mut buf).await.unwrap();
                            if n == 0 {
                                break;
                            }
                            sw.write_all(&buf[..n]).await.unwrap();
                            remaining -= n;
                        }
                        sw.shutdown().await.unwrap();
                        black_box(echo_payload);
                    });

                    cw.write_all(&payload).await.unwrap();
                    cw.shutdown().await.unwrap();

                    let mut recv = Vec::with_capacity(size);
                    let mut buf = vec![0u8; size.min(32 * 1024)];
                    let mut remaining = size;
                    while remaining > 0 {
                        let n = cr.read(&mut buf).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        recv.extend_from_slice(&buf[..n]);
                        remaining -= n;
                    }
                    let _ = echo.await;
                    client.close();
                    server.close();
                    black_box(recv.len());
                }
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_mux_stream_write,
    bench_event_write_data,
    bench_event_read_data,
    bench_window_update_roundtrip,
    bench_connection_duplex,
);
criterion_main!(benches);
