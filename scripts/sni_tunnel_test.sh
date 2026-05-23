#!/usr/bin/env bash
# SNI tunnel integration smoke test.
#
# Verifies that multiple tunnels on the same remote port are routed by TLS SNI,
# and that plain TCP falls back to the default (no-SNI) route.
#
# Usage (from repo root):
#   ./scripts/sni_tunnel_test.sh
#
# Requires: bash, python3, nc, rg (or grep), lsof (for cleanup)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SERVER_LISTEN="127.0.0.1:48100"
ADMIN_LISTEN="127.0.0.1:48107"
REMOTE_PORT=8443
TUNNEL_CLIENT_ID="sni-test-$$"
LOG_DIR="${TMPDIR:-/tmp}/rsnova-sni-test-$$"
CERT="$ROOT/cert.pem"
KEY="$ROOT/key.pem"

PIDS=()

log() {
  printf '[sni-test] %s\n' "$*"
}

fail() {
  log "FAIL: $*"
  exit 1
}

cleanup() {
  local pid
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  lsof -ti:"$SERVER_LISTEN" "$ADMIN_LISTEN" "$REMOTE_PORT" 127.0.0.1:9990 \
    127.0.0.1:9991 127.0.0.1:9992 2>/dev/null | xargs kill -9 2>/dev/null || true
  rm -rf "$LOG_DIR"
}
trap cleanup EXIT INT TERM

mkdir -p "$LOG_DIR"

resolve_rsnova() {
  cargo build --release --quiet
  local bin="${CARGO_TARGET_DIR:-$ROOT/target}/release/rsnova"
  if [ ! -x "$bin" ]; then
    fail "rsnova binary not found at $bin"
  fi
  if ! "$bin" --help 2>&1 | grep -q 'tunnel-client-id'; then
    fail "rsnova at $bin does not support tunnel mode (stale binary?)"
  fi
  echo "$bin"
}

ensure_certs() {
  if [ ! -f "$CERT" ] || [ ! -f "$KEY" ]; then
    log "generating self-signed cert for localhost"
    "$RSNOVA" --rcgen true --tls-host localhost
  fi
}

start_backend() {
  local port=$1
  local response=$2
  python3 - "$port" "$response" <<'PY' &
import socket
import sys

port = int(sys.argv[1])
response = sys.argv[2].encode()
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port))
s.listen(8)
while True:
    conn, _ = s.accept()
    conn.recv(4096)
    conn.sendall(response)
    conn.close()
PY
  PIDS+=($!)
}

wait_for_log() {
  local file=$1
  local pattern=$2
  local tries=${3:-30}
  while [ "$tries" -gt 0 ]; do
    if grep -q "$pattern" "$file" 2>/dev/null; then
      return 0
    fi
    sleep 0.2
    tries=$((tries - 1))
  done
  return 1
}

tls_connect_with_sni() {
  local sni=$1
  python3 - "$sni" <<'PY' || true
import socket
import ssl
import sys

sni = sys.argv[1]
sock = socket.create_connection(("127.0.0.1", 8443), timeout=5)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE
try:
    wrapped = ctx.wrap_socket(sock, server_hostname=sni)
    wrapped.settimeout(1)
    try:
        wrapped.recv(64)
    except Exception:
        pass
    wrapped.close()
except Exception:
    pass
PY
}

RSNOVA="$(resolve_rsnova)"
ensure_certs

log "starting backends on 9990 (default), 9991 (api), 9992 (web)"
start_backend 9990 "default:ping"
start_backend 9991 "api-ok"
start_backend 9992 "web-ok"
sleep 0.5

log "starting server on $SERVER_LISTEN"
"$RSNOVA" --role server --listen "$SERVER_LISTEN" \
  --key "$KEY" --cert "$CERT" --tunnel-port-range 8000-9000 \
  >"$LOG_DIR/server.log" 2>&1 &
PIDS+=($!)
sleep 1

log "starting tunnel client (3 routes on port $REMOTE_PORT)"
"$RSNOVA" --role client --remote "tls://$SERVER_LISTEN" --cert "$CERT" --tls-host localhost \
  --tunnel-client-id "$TUNNEL_CLIENT_ID" \
  --tunnel "localhost:9990:$REMOTE_PORT" \
  --tunnel "localhost:9991:$REMOTE_PORT:api.example.com" \
  --tunnel "localhost:9992:$REMOTE_PORT:web.example.com" \
  --admin-listen "$ADMIN_LISTEN" \
  >"$LOG_DIR/client.log" 2>&1 &
PIDS+=($!)

wait_for_log "$LOG_DIR/client.log" "Tunnel registered: :$REMOTE_PORT (SNI: api.example.com)" \
  || fail "api SNI tunnel did not register (see $LOG_DIR/client.log)"
wait_for_log "$LOG_DIR/client.log" "Tunnel registered: :$REMOTE_PORT (SNI: web.example.com)" \
  || fail "web SNI tunnel did not register (see $LOG_DIR/client.log)"
wait_for_log "$LOG_DIR/client.log" "Tunnel registered: :$REMOTE_PORT → OK" \
  || fail "default tunnel did not register (see $LOG_DIR/client.log)"

log "testing default route (plain TCP)"
DEFAULT=$(printf 'ping' | nc -w 3 127.0.0.1 "$REMOTE_PORT" || true)
[ "$DEFAULT" = "default:ping" ] || fail "default route expected 'default:ping', got '$DEFAULT'"

log "testing SNI route api.example.com"
tls_connect_with_sni api.example.com
sleep 0.5
grep -q "Reverse stream: connecting to localhost:9991" "$LOG_DIR/client.log" \
  || fail "api SNI did not route to localhost:9991 (see $LOG_DIR/client.log)"

log "testing SNI route web.example.com"
tls_connect_with_sni web.example.com
sleep 0.5
grep -q "Reverse stream: connecting to localhost:9992" "$LOG_DIR/client.log" \
  || fail "web SNI did not route to localhost:9992 (see $LOG_DIR/client.log)"

grep -q "Reverse stream: connecting to localhost:9990" "$LOG_DIR/client.log" \
  || fail "default route did not reach localhost:9990 (see $LOG_DIR/client.log)"

log "PASS: SNI routing smoke test"
log "logs: $LOG_DIR"
