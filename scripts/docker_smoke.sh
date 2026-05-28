#!/usr/bin/env bash
# Docker integration smoke test (Phase 1).
#
# Builds the release-based image, initializes data with a locally built binary
# (--init-dir is not yet in published releases), starts server + proxy, and
# verifies TLS proxy connectivity.
#
# Usage (from repo root):
#   ./scripts/docker_smoke.sh
#
# Requires: bash, docker, docker compose, curl, cargo

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DATA_DIR="${TMPDIR:-/tmp}/rsnova-docker-smoke-$$"
COMPOSE="docker compose"
VERSION="${RSNOVA_RELEASE_VERSION:-v0.1.0}"
RSNOVA_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/rsnova"

log() {
  printf '[docker-smoke] %s\n' "$*"
}

fail() {
  log "FAIL: $*"
  exit 1
}

cleanup() {
  $COMPOSE --profile server --profile client_proxy down --remove-orphans >/dev/null 2>&1 || true
  rm -rf "$DATA_DIR"
}

trap cleanup EXIT

log "Building local rsnova binary for --init-dir"
cargo build --release --locked

log "Initializing data directory at $DATA_DIR"
mkdir -p "$DATA_DIR"
"$RSNOVA_BIN" --init-dir "$DATA_DIR" < /dev/null

for file in cert.pem key.pem server.toml client_proxy.toml client_tunnel.toml; do
  [[ -f "$DATA_DIR/$file" ]] || fail "missing init output: $file"
done

log "Building Docker image (RSNOVA_RELEASE_VERSION=$VERSION, RSNOVA_IMAGE_TAG=$VERSION)"
RSNOVA_RELEASE_VERSION="$VERSION" RSNOVA_IMAGE_TAG="$VERSION" $COMPOSE build

log "Starting server and proxy"
RSNOVA_DATA="$DATA_DIR" $COMPOSE --profile server --profile client_proxy up -d

wait_for_port() {
  local host="$1"
  local port="$2"
  local tries="${3:-30}"
  local i
  for ((i = 1; i <= tries; i++)); do
    if curl -sS --max-time 1 "http://${host}:${port}/metrics" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

wait_for_port 127.0.0.1 48102 || fail "server admin port 48102 not ready"
wait_for_port 127.0.0.1 48103 || fail "proxy admin port 48103 not ready"

log "Verifying proxy HTTP CONNECT through TLS tunnel"
response="$(curl -sS --max-time 10 -x http://127.0.0.1:48101 http://example.com/ -o /dev/null -w '%{http_code}')"
[[ "$response" == "200" ]] || fail "expected HTTP 200 via proxy, got $response"

log "PASS"
