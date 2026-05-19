#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULT_ROOT="${RESULT_ROOT:-$PROJECT_ROOT/target/bench-baseline}"
RUN_ID="${RUN_ID:-$(date +%Y%m%d-%H%M%S)}"
RESULT_DIR="$RESULT_ROOT/$RUN_ID"
BIN="${BIN:-$PROJECT_ROOT/target/release/rsnova}"
CERT_DIR="$RESULT_DIR/certs"
HELPER="$RESULT_DIR/bench_helper.py"

TLS_HOST="${TLS_HOST:-localhost}"
REMOTE_LISTEN="${REMOTE_LISTEN:-127.0.0.1:48200}"
LOCAL_LISTEN="${LOCAL_LISTEN:-127.0.0.1:48100}"
BACKEND_LISTEN="${BACKEND_LISTEN:-127.0.0.1:48300}"
SERVER_ADMIN="${SERVER_ADMIN:-127.0.0.1:48202}"
CLIENT_ADMIN="${CLIENT_ADMIN:-127.0.0.1:48203}"
THREADS="${THREADS:-2}"
CONCURRENT="${CONCURRENT:-5}"

BENCH_BYTES="${BENCH_BYTES:-268435456}"               # 256MiB
CONCURRENT_CONNECTIONS="${CONCURRENT_CONNECTIONS:-16}"
CONCURRENT_BYTES="${CONCURRENT_BYTES:-67108864}"     # 64MiB per connection
SMALL_PACKET_ITERS="${SMALL_PACKET_ITERS:-10000}"
SMALL_PACKET_SIZE="${SMALL_PACKET_SIZE:-64}"
IDLE_CONNS="${IDLE_CONNS:-200}"
IDLE_SECS="${IDLE_SECS:-15}"
SKIP_BUILD="${SKIP_BUILD:-0}"
RUN_FILE_LOG_CASE="${RUN_FILE_LOG_CASE:-0}"

SERVER_PID=""
CLIENT_PID=""
BACKEND_PID=""

mkdir -p "$RESULT_DIR" "$CERT_DIR"

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required command: $1" >&2
    exit 1
  }
}

cleanup() {
  for pid in "$CLIENT_PID" "$SERVER_PID" "$BACKEND_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" >/dev/null 2>&1; then
      kill "$pid" >/dev/null 2>&1 || true
    fi
  done
  wait >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

require_cmd cargo
require_cmd python3

cat > "$HELPER" <<'PY'
#!/usr/bin/env python3
import argparse
import concurrent.futures
import json
import socket
import socketserver
import statistics
import sys
import threading
import time
import urllib.parse

CHUNK = b"\0" * 65536


def parse_addr(value):
    host, port = value.rsplit(":", 1)
    return host, int(port)


def read_exact(sock, n):
    data = bytearray()
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise RuntimeError("connection closed")
        data.extend(chunk)
    return bytes(data)


def read_line(sock):
    data = bytearray()
    while True:
        c = sock.recv(1)
        if not c:
            raise RuntimeError("connection closed while reading line")
        data.extend(c)
        if c == b"\n":
            return bytes(data).decode("ascii").strip()


def socks_connect(proxy, target):
    sock = socket.create_connection(parse_addr(proxy), timeout=10)
    sock.settimeout(None)
    sock.sendall(b"\x05\x01\x00")
    resp = read_exact(sock, 2)
    if resp != b"\x05\x00":
        raise RuntimeError(f"SOCKS auth failed: {resp!r}")

    host, port = parse_addr(target)
    try:
        addr = socket.inet_aton(host)
        req = b"\x05\x01\x00\x01" + addr + port.to_bytes(2, "big")
    except OSError:
        host_b = host.encode("idna")
        req = b"\x05\x01\x00\x03" + bytes([len(host_b)]) + host_b + port.to_bytes(2, "big")
    sock.sendall(req)
    head = read_exact(sock, 4)
    if head[1] != 0:
        raise RuntimeError(f"SOCKS connect failed: {head!r}")
    atyp = head[3]
    if atyp == 1:
        read_exact(sock, 4)
    elif atyp == 3:
        ln = read_exact(sock, 1)[0]
        read_exact(sock, ln)
    elif atyp == 4:
        read_exact(sock, 16)
    read_exact(sock, 2)
    return sock


class BenchHandler(socketserver.BaseRequestHandler):
    def handle(self):
        try:
            line = read_line(self.request)
            parts = line.split()
            if not parts:
                return
            cmd = parts[0]
            if cmd == "UPLOAD":
                remaining = int(parts[1])
                while remaining > 0:
                    chunk = self.request.recv(min(65536, remaining))
                    if not chunk:
                        return
                    remaining -= len(chunk)
                self.request.sendall(b"OK\n")
            elif cmd == "DOWNLOAD":
                remaining = int(parts[1])
                while remaining > 0:
                    n = min(len(CHUNK), remaining)
                    self.request.sendall(CHUNK[:n])
                    remaining -= n
            elif cmd == "PING":
                iters = int(parts[1])
                size = int(parts[2])
                for _ in range(iters):
                    payload = read_exact(self.request, size)
                    self.request.sendall(payload)
            else:
                self.request.sendall(b"ERR\n")
        except Exception as exc:
            try:
                self.request.sendall(f"ERR {exc}\n".encode())
            except Exception:
                pass


class ThreadingTCPServer(socketserver.ThreadingMixIn, socketserver.TCPServer):
    allow_reuse_address = True
    daemon_threads = True


def serve_backend(listen):
    with ThreadingTCPServer(parse_addr(listen), BenchHandler) as server:
        server.serve_forever()


def bench_upload(proxy, target, total_bytes):
    sock = socks_connect(proxy, target)
    try:
        sock.sendall(f"UPLOAD {total_bytes}\n".encode())
        remaining = total_bytes
        while remaining > 0:
            n = min(len(CHUNK), remaining)
            sock.sendall(CHUNK[:n])
            remaining -= n
        if read_line(sock) != "OK":
            raise RuntimeError("upload ack failed")
    finally:
        sock.close()


def bench_download(proxy, target, total_bytes):
    sock = socks_connect(proxy, target)
    try:
        sock.sendall(f"DOWNLOAD {total_bytes}\n".encode())
        remaining = total_bytes
        while remaining > 0:
            data = sock.recv(min(65536, remaining))
            if not data:
                raise RuntimeError("download closed early")
            remaining -= len(data)
    finally:
        sock.close()


def timed(fn, *args):
    start = time.perf_counter()
    fn(*args)
    return time.perf_counter() - start


def output(result):
    print(json.dumps(result, sort_keys=True), flush=True)


def run_single(args, mode):
    total = args.bytes
    elapsed = timed(bench_upload if mode == "upload" else bench_download, args.proxy, args.target, total)
    output({
        "mode": mode,
        "bytes": total,
        "duration_sec": elapsed,
        "throughput_mib_s": total / 1024 / 1024 / elapsed,
    })


def run_concurrent(args, mode):
    total = args.connections * args.bytes_per_conn
    fn = bench_upload if mode == "concurrent-upload" else bench_download
    start = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.connections) as pool:
        futs = [pool.submit(fn, args.proxy, args.target, args.bytes_per_conn) for _ in range(args.connections)]
        for fut in concurrent.futures.as_completed(futs):
            fut.result()
    elapsed = time.perf_counter() - start
    output({
        "mode": mode,
        "connections": args.connections,
        "bytes_per_conn": args.bytes_per_conn,
        "bytes": total,
        "duration_sec": elapsed,
        "throughput_mib_s": total / 1024 / 1024 / elapsed,
    })


def run_small(args):
    sock = socks_connect(args.proxy, args.target)
    latencies_us = []
    payload = b"x" * args.size
    try:
        sock.sendall(f"PING {args.iters} {args.size}\n".encode())
        for _ in range(args.iters):
            start = time.perf_counter_ns()
            sock.sendall(payload)
            read_exact(sock, args.size)
            latencies_us.append((time.perf_counter_ns() - start) / 1000)
    finally:
        sock.close()
    sorted_lats = sorted(latencies_us)
    output({
        "mode": "small-packet-rtt",
        "iters": args.iters,
        "size": args.size,
        "avg_us": statistics.fmean(latencies_us),
        "p50_us": sorted_lats[int(len(sorted_lats) * 0.50)],
        "p95_us": sorted_lats[int(len(sorted_lats) * 0.95)],
        "p99_us": sorted_lats[int(len(sorted_lats) * 0.99)],
    })


def run_idle(args):
    socks = []
    for _ in range(args.connections):
        socks.append(socks_connect(args.proxy, args.target))
    time.sleep(args.seconds)
    for sock in socks:
        sock.close()
    output({"mode": "idle", "connections": args.connections, "seconds": args.seconds})


def wait_port(addr, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(parse_addr(addr), timeout=1):
                output({"mode": "wait-port", "addr": addr, "ready": True})
                return
        except OSError:
            time.sleep(0.1)
    raise SystemExit(f"timeout waiting for {addr}")


def wait_socks(addr, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        sock = None
        try:
            sock = socket.create_connection(parse_addr(addr), timeout=1)
            sock.sendall(b"\x05\x01\x00")
            if read_exact(sock, 2) == b"\x05\x00":
                output({"mode": "wait-socks", "addr": addr, "ready": True})
                return
        except OSError:
            time.sleep(0.1)
        except RuntimeError:
            time.sleep(0.1)
        finally:
            if sock is not None:
                sock.close()
    raise SystemExit(f"timeout waiting for SOCKS proxy {addr}")


def fetch(url):
    parsed = urllib.parse.urlparse(url)
    host = parsed.hostname or "127.0.0.1"
    port = parsed.port or 80
    path = parsed.path or "/"
    if parsed.query:
        path += "?" + parsed.query
    with socket.create_connection((host, port), timeout=5) as sock:
        req = f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
        sock.sendall(req.encode("ascii"))
        response = bytearray()
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                break
            response.extend(chunk)
    marker = b"\r\n\r\n"
    idx = response.find(marker)
    sys.stdout.buffer.write(response[idx + len(marker):] if idx >= 0 else response)


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("backend")
    p.add_argument("--listen", required=True)

    p = sub.add_parser("wait-port")
    p.add_argument("--addr", required=True)
    p.add_argument("--timeout", type=float, default=10)

    p = sub.add_parser("wait-socks")
    p.add_argument("--addr", required=True)
    p.add_argument("--timeout", type=float, default=10)

    p = sub.add_parser("fetch")
    p.add_argument("--url", required=True)

    for name in ["upload", "download"]:
        p = sub.add_parser(name)
        p.add_argument("--proxy", required=True)
        p.add_argument("--target", required=True)
        p.add_argument("--bytes", type=int, required=True)

    for name in ["concurrent-upload", "concurrent-download"]:
        p = sub.add_parser(name)
        p.add_argument("--proxy", required=True)
        p.add_argument("--target", required=True)
        p.add_argument("--connections", type=int, required=True)
        p.add_argument("--bytes-per-conn", type=int, required=True)

    p = sub.add_parser("small")
    p.add_argument("--proxy", required=True)
    p.add_argument("--target", required=True)
    p.add_argument("--iters", type=int, required=True)
    p.add_argument("--size", type=int, required=True)

    p = sub.add_parser("idle")
    p.add_argument("--proxy", required=True)
    p.add_argument("--target", required=True)
    p.add_argument("--connections", type=int, required=True)
    p.add_argument("--seconds", type=int, required=True)

    args = parser.parse_args()
    if args.cmd == "backend":
        serve_backend(args.listen)
    elif args.cmd == "wait-port":
        wait_port(args.addr, args.timeout)
    elif args.cmd == "wait-socks":
        wait_socks(args.addr, args.timeout)
    elif args.cmd == "fetch":
        fetch(args.url)
    elif args.cmd in ("upload", "download"):
        run_single(args, args.cmd)
    elif args.cmd in ("concurrent-upload", "concurrent-download"):
        run_concurrent(args, args.cmd)
    elif args.cmd == "small":
        run_small(args)
    elif args.cmd == "idle":
        run_idle(args)


if __name__ == "__main__":
    main()
PY
chmod +x "$HELPER"

if [[ "$SKIP_BUILD" != "1" ]]; then
  cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" | tee "$RESULT_DIR/build.log"
fi

if [[ ! -x "$BIN" ]]; then
  echo "binary not found or not executable: $BIN" >&2
  exit 1
fi

(
  cd "$CERT_DIR"
  "$BIN" --rcgen --tls-host "$TLS_HOST" > "$RESULT_DIR/rcgen.log" 2>&1
)

sample_processes() {
  local label="$1"
  {
    echo "===== $label $(date -u +%Y-%m-%dT%H:%M:%SZ) ====="
    for pid in "$SERVER_PID" "$CLIENT_PID" "$BACKEND_PID"; do
      if [[ -n "$pid" ]] && kill -0 "$pid" >/dev/null 2>&1; then
        ps -o pid,ppid,pcpu,rss,command -p "$pid"
      fi
    done
  } >> "$RESULT_DIR/process_stats.txt" 2>&1 || true
}

fetch_metrics() {
  local label="$1"
  python3 "$HELPER" fetch --url "http://$CLIENT_ADMIN/metrics" > "$RESULT_DIR/metrics_client_${label}.txt" 2>/dev/null || true
  python3 "$HELPER" fetch --url "http://$SERVER_ADMIN/metrics" > "$RESULT_DIR/metrics_server_${label}.txt" 2>/dev/null || true
}

run_case() {
  local name="$1"
  shift
  echo "running $name"
  sample_processes "before_$name"
  fetch_metrics "before_$name"
  python3 "$HELPER" "$@" | tee "$RESULT_DIR/$name.json"
  fetch_metrics "after_$name"
  sample_processes "after_$name"
}

python3 "$HELPER" backend --listen "$BACKEND_LISTEN" > "$RESULT_DIR/backend.log" 2>&1 &
BACKEND_PID=$!
python3 "$HELPER" wait-port --addr "$BACKEND_LISTEN" --timeout 10 >> "$RESULT_DIR/startup.log"

"$BIN" \
  --role server \
  --protocol tls \
  --listen "$REMOTE_LISTEN" \
  --admin-listen "$SERVER_ADMIN" \
  --cert "$CERT_DIR/cert.pem" \
  --key "$CERT_DIR/key.pem" \
  --tls-host "$TLS_HOST" \
  --threads "$THREADS" \
  > "$RESULT_DIR/rsnova-server.log" 2>&1 &
SERVER_PID=$!
python3 "$HELPER" wait-port --addr "$REMOTE_LISTEN" --timeout 10 >> "$RESULT_DIR/startup.log"
python3 "$HELPER" wait-port --addr "$SERVER_ADMIN" --timeout 10 >> "$RESULT_DIR/startup.log"

"$BIN" \
  --role client \
  --protocol tls \
  --listen "$LOCAL_LISTEN" \
  --admin-listen "$CLIENT_ADMIN" \
  --remote "tls://$REMOTE_LISTEN" \
  --cert "$CERT_DIR/cert.pem" \
  --tls-host "$TLS_HOST" \
  --concurrent "$CONCURRENT" \
  --threads "$THREADS" \
  > "$RESULT_DIR/rsnova-client.log" 2>&1 &
CLIENT_PID=$!
python3 "$HELPER" wait-socks --addr "$LOCAL_LISTEN" --timeout 10 >> "$RESULT_DIR/startup.log"
python3 "$HELPER" wait-port --addr "$CLIENT_ADMIN" --timeout 10 >> "$RESULT_DIR/startup.log"

cat > "$RESULT_DIR/config.txt" <<EOF
PROJECT_ROOT=$PROJECT_ROOT
BIN=$BIN
TLS_HOST=$TLS_HOST
REMOTE_LISTEN=$REMOTE_LISTEN
LOCAL_LISTEN=$LOCAL_LISTEN
BACKEND_LISTEN=$BACKEND_LISTEN
SERVER_ADMIN=$SERVER_ADMIN
CLIENT_ADMIN=$CLIENT_ADMIN
THREADS=$THREADS
CONCURRENT=$CONCURRENT
BENCH_BYTES=$BENCH_BYTES
CONCURRENT_CONNECTIONS=$CONCURRENT_CONNECTIONS
CONCURRENT_BYTES=$CONCURRENT_BYTES
SMALL_PACKET_ITERS=$SMALL_PACKET_ITERS
SMALL_PACKET_SIZE=$SMALL_PACKET_SIZE
IDLE_CONNS=$IDLE_CONNS
IDLE_SECS=$IDLE_SECS
RUN_FILE_LOG_CASE=$RUN_FILE_LOG_CASE
EOF

sample_processes "startup"
fetch_metrics "startup"

run_case "upload_single" upload --proxy "$LOCAL_LISTEN" --target "$BACKEND_LISTEN" --bytes "$BENCH_BYTES"
run_case "download_single" download --proxy "$LOCAL_LISTEN" --target "$BACKEND_LISTEN" --bytes "$BENCH_BYTES"
run_case "concurrent_upload" concurrent-upload --proxy "$LOCAL_LISTEN" --target "$BACKEND_LISTEN" --connections "$CONCURRENT_CONNECTIONS" --bytes-per-conn "$CONCURRENT_BYTES"
run_case "concurrent_download" concurrent-download --proxy "$LOCAL_LISTEN" --target "$BACKEND_LISTEN" --connections "$CONCURRENT_CONNECTIONS" --bytes-per-conn "$CONCURRENT_BYTES"
run_case "small_packet_rtt" small --proxy "$LOCAL_LISTEN" --target "$BACKEND_LISTEN" --iters "$SMALL_PACKET_ITERS" --size "$SMALL_PACKET_SIZE"
run_case "idle_connections" idle --proxy "$LOCAL_LISTEN" --target "$BACKEND_LISTEN" --connections "$IDLE_CONNS" --seconds "$IDLE_SECS"

if [[ "$RUN_FILE_LOG_CASE" == "1" ]]; then
  echo "file log comparison is enabled; restarting rsnova with --log"
  kill "$CLIENT_PID" "$SERVER_PID" >/dev/null 2>&1 || true
  wait "$CLIENT_PID" "$SERVER_PID" >/dev/null 2>&1 || true

  "$BIN" \
    --role server \
    --protocol tls \
    --listen "$REMOTE_LISTEN" \
    --admin-listen "$SERVER_ADMIN" \
    --cert "$CERT_DIR/cert.pem" \
    --key "$CERT_DIR/key.pem" \
    --tls-host "$TLS_HOST" \
    --threads "$THREADS" \
    --log "$RESULT_DIR/server-file.log" \
    > "$RESULT_DIR/rsnova-server-filelog.stdout" 2>&1 &
  SERVER_PID=$!
  python3 "$HELPER" wait-port --addr "$REMOTE_LISTEN" --timeout 10 >> "$RESULT_DIR/startup_filelog.log"

  "$BIN" \
    --role client \
    --protocol tls \
    --listen "$LOCAL_LISTEN" \
    --admin-listen "$CLIENT_ADMIN" \
    --remote "tls://$REMOTE_LISTEN" \
    --cert "$CERT_DIR/cert.pem" \
    --tls-host "$TLS_HOST" \
    --concurrent "$CONCURRENT" \
    --threads "$THREADS" \
    --log "$RESULT_DIR/client-file.log" \
    > "$RESULT_DIR/rsnova-client-filelog.stdout" 2>&1 &
  CLIENT_PID=$!
  python3 "$HELPER" wait-port --addr "$LOCAL_LISTEN" --timeout 10 >> "$RESULT_DIR/startup_filelog.log"

  run_case "upload_single_filelog" upload --proxy "$LOCAL_LISTEN" --target "$BACKEND_LISTEN" --bytes "$BENCH_BYTES"
fi

cat > "$RESULT_DIR/summary.txt" <<EOF
Baseline finished.

Result directory:
  $RESULT_DIR

Key result files:
  upload_single.json
  download_single.json
  concurrent_upload.json
  concurrent_download.json
  small_packet_rtt.json
  idle_connections.json
  process_stats.txt
  metrics_client_*.txt
  metrics_server_*.txt
  rsnova-client.log
  rsnova-server.log
EOF

cat "$RESULT_DIR/summary.txt"
