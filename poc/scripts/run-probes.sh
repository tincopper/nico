#!/usr/bin/env bash
# One-shot reproduction script for the Pingora 0.9.1 capability probes.
#
# Every finding in FINDINGS.md is produced by one of the sections below.
# Raw logs land in $OUT/logs so a reviewer can grep them.
#
#   ./scripts/run-probes.sh                    # build + run everything
#   OUT=/tmp/pingora-evidence ./scripts/run-probes.sh
#
# Ports: gateway 18080 (h1/h2), 18082 (4 workers), 18084 (graceful),
#        mock 18081.

set -uo pipefail
cd "$(dirname "$0")/.."

OUT=${OUT:-./.evidence}
rm -rf "$OUT"
mkdir -p "$OUT/logs"
BIN=./target/release
GW_PORT=18080
MOCK_PORT=18081
GW_PID=""
MOCK_PID=""

log() { printf '%s\n' "$*" | tee -a "$OUT/summary.txt"; }
section() { printf '\n=== %s ===\n' "$*" | tee -a "$OUT/summary.txt"; }

cleanup() {
  stop_gateway
  if [[ -n "${MOCK_PID:-}" ]]; then kill -9 "$MOCK_PID" 2>/dev/null; fi
  return 0
}
trap cleanup EXIT

wait_http() {
  for _ in $(seq 1 100); do
    if curl -sS --max-time 0.5 -o /dev/null "http://127.0.0.1:$1/hello" 2>/dev/null; then return 0; fi
    sleep 0.1
  done
  echo "gateway on $1 did not become ready" >&2
  return 1
}

stop_gateway() {
  if [[ -n "${GW_PID:-}" ]]; then
    kill -9 "$GW_PID" 2>/dev/null
    for _ in $(seq 1 50); do kill -0 "$GW_PID" 2>/dev/null || break; sleep 0.05; done
    GW_PID=""
  fi
  return 0
}

start_mock() {
  SPIKE_MOCK_ADDR=127.0.0.1:$MOCK_PORT "$BIN/spike-mock" >"$OUT/logs/mock.log" 2>&1 &
  MOCK_PID=$!
  for _ in $(seq 1 50); do
    grep -q "mock.listen" "$OUT/logs/mock.log" 2>/dev/null && return 0
    sleep 0.1
  done
  return 1
}

start_gateway() { # $1=port $2=grace $3=threads $4=logfile
  stop_gateway
  SPIKE_LISTEN=127.0.0.1:"$1" \
  SPIKE_UPSTREAM=127.0.0.1:$MOCK_PORT \
  SPIKE_GRACE="$2" \
  SPIKE_THREADS="${3:-1}" \
    "$BIN/spike-http" >"$4" 2>&1 &
  GW_PID=$!
  wait_http "$1"
}

echo "== building (all features) ==" | tee "$OUT/summary.txt"
cargo build --release --all-features 2>&1 | tail -2 | tee -a "$OUT/summary.txt"

start_mock
start_gateway "$GW_PORT" 3 1 "$OUT/logs/gateway.log"
log "gateway pid=$GW_PID mock pid=$MOCK_PID"

##############################################################################
section "1. H1/H2 on the same port"
{
  echo "--- curl --http1.1 /hello"
  curl -sS --http1.1 -D - -o /dev/null "http://127.0.0.1:$GW_PORT/hello"
  echo "--- curl --http2-prior-knowledge /hello"
  curl -sS --http2-prior-knowledge -D - -o /dev/null "http://127.0.0.1:$GW_PORT/hello"
} 2>&1 | tee "$OUT/1-h1-h2.txt"

##############################################################################
section "2. request/response body hooks (8 MiB, key bytes at the end)"
python3 - <<'PY'
data = b"0123456789abcdef" * (8 * 1024 * 1024 // 16)
data = data[: -len(b"TAIL-SENTINEL-8MiB")] + b"TAIL-SENTINEL-8MiB"
open("/tmp/spike-8mib.bin", "wb").write(data)
open("/tmp/spike-1mib.bin", "wb").write(b"abcdefgh" * 131072)
PY
{
  echo "upload fixture sha256: $(shasum -a 256 /tmp/spike-8mib.bin | cut -d' ' -f1)"
  echo "--- POST /upload, content-length, 8 MiB, sentinel at the end"
  curl -sS -X POST --data-binary @/tmp/spike-8mib.bin \
    -H "content-type: application/octet-stream" "http://127.0.0.1:$GW_PORT/upload"
  echo
  echo "--- POST /upload, chunked, 1 MiB (no content-length)"
  curl -sS -X POST -H "Expect:" -H "Transfer-Encoding: chunked" \
    --data-binary @/tmp/spike-1mib.bin "http://127.0.0.1:$GW_PORT/upload"
  echo
  echo "--- GET /download 8 MiB"
  curl -sS -D /tmp/dl.hdr -o /tmp/spike-dl.bin "http://127.0.0.1:$GW_PORT/download?mb=8"
  echo "mock-declared sha256: $(grep -i 'x-mock-sha256' /tmp/dl.hdr | tr -d '\r' | awk '{print $2}')"
  echo "client-received sha256: $(shasum -a 256 /tmp/spike-dl.bin | cut -d' ' -f1)"
} 2>&1 | tee "$OUT/2-body.txt"
grep -a "spike.done" "$OUT/logs/gateway.log" | tail -3 \
  | sed -E 's/^[^ ]+ +//' | tee "$OUT/2-body-hooks.txt"

##############################################################################
section "3. routing metadata late: body arrives after the upstream request"
{
  echo "--- 1 MiB upload trickled at 128 KiB/s (8s), Expect suppressed"
  time curl -sS -X POST -H "Expect:" --limit-rate 128k --data-binary @/tmp/spike-1mib.bin \
    "http://127.0.0.1:$GW_PORT/upload"
  echo
  echo "--- mock: request head long before the body is complete"
  grep -a "mock.req.head\|mock.req.body" "$OUT/logs/mock.log" | tail -2 | sed -E 's/^[^ ]+ +//'
} 2>&1 | tee "$OUT/3-late-metadata.txt"
{
  echo "--- gateway: upstream connected at elapsed_ms=0, body streamed for ~8s"
  grep -a "spike.upstream.connected" "$OUT/logs/gateway.log" | tail -1 | sed -E 's/^[^ ]+ +//'
  grep -a "spike.done" "$OUT/logs/gateway.log" | tail -1 | sed -E 's/^[^ ]+ +//'
} 2>&1 | tee -a "$OUT/3-late-metadata.txt"

##############################################################################
section "4. SSE forwarding and fragmentation"
{
  echo "--- /sse 5 events x 200ms (client arrival timestamps)"
  curl -sS -N --trace-time --trace-ascii /tmp/sse.trace -o /tmp/sse.txt \
    "http://127.0.0.1:$GW_PORT/sse?n=5&delay_ms=200"
  cat /tmp/sse.txt
  grep "Recv data" /tmp/sse.trace
  echo "--- /sse over downstream h2 (client arrival timestamps)"
  curl -sS -N --http2-prior-knowledge --trace-time --trace-ascii /tmp/sse-h2.trace \
    -o /tmp/sse-h2.txt "http://127.0.0.1:$GW_PORT/sse?n=3&delay_ms=200"
  cat /tmp/sse-h2.txt
  grep "Recv data" /tmp/sse-h2.trace
  echo "--- fragmented writes (1 / 3 / random byte chunks) must reassemble identically"
  for m in 1 3 rand; do
    curl -sS -N -o "/tmp/sse-frag-$m.txt" "http://127.0.0.1:$GW_PORT/sse-frag?n=2&delay_ms=0&mode=$m"
    echo "mode=$m sha256=$(shasum -a 256 "/tmp/sse-frag-$m.txt" | cut -d' ' -f1)"
  done
  printf 'data: event-0\n\ndata: event-1\n\ndata: [DONE]\n\n' > /tmp/sse-expected.txt
  echo "expected  sha256=$(shasum -a 256 /tmp/sse-expected.txt | cut -d' ' -f1)"
} 2>&1 | tee "$OUT/4-sse.txt"
grep -a "spike.done" "$OUT/logs/gateway.log" | tail -5 \
  | sed -E 's/^[^ ]+ +//' | tee -a "$OUT/4-sse.txt"

##############################################################################
section "5. slow-client backpressure (32 MiB at 2 MiB/s)"
{
  echo "--- gateway RSS samples while the client reads slowly"
  (
    for _ in $(seq 1 10); do
      echo "rss_kb=$(ps -o rss= -p "$GW_PID" | tr -d ' ')"
      sleep 1.5
    done
  ) > /tmp/rss.log &
  RSS_PID=$!
  curl -sS --limit-rate 2m -o /dev/null \
    -w 'downloaded=%{size_download} time=%{time_total}\n' \
    "http://127.0.0.1:$GW_PORT/flood?mb=32"
  wait $RSS_PID
  echo "rss samples: $(tr '\n' ' ' < /tmp/rss.log)"
  echo "--- mock write progress (each mark is 8 MiB, blocked by the slow client)"
  grep -a "mock.flood" "$OUT/logs/mock.log" | tail -6 | sed -E 's/^[^ ]+ +//'
} 2>&1 | tee "$OUT/5-backpressure.txt"

##############################################################################
section "6. client cancel propagates upstream"
{
  echo "--- curl --max-time 0.5 on an endless stream"
  curl -sS --max-time 0.5 -o /dev/null "http://127.0.0.1:$GW_PORT/cancel"
  echo "curl exit=$?"
  sleep 1
  grep -a "mock.cancel" "$OUT/logs/mock.log" | tail -3 | sed -E 's/^[^ ]+ +//'
} 2>&1 | tee "$OUT/6-cancel.txt"
grep -a "spike.done" "$OUT/logs/gateway.log" | tail -1 | sed -E 's/^[^ ]+ +//' | tee -a "$OUT/6-cancel.txt"

##############################################################################
section "7. connection-pool scope"
{
  echo "--- 5 sequential requests, separate downstream connections"
  for _ in 1 2 3 4 5; do
    curl -sS -D - -o /dev/null "http://127.0.0.1:$GW_PORT/pool" | grep -i "x-mock-peer-port"
  done
  echo "--- h1 and h2 downstream share the same upstream pool"
  curl -sS --http1.1 -D - -o /dev/null "http://127.0.0.1:$GW_PORT/hello" | grep -i "x-mock-peer-port"
  curl -sS --http2-prior-knowledge -D - -o /dev/null "http://127.0.0.1:$GW_PORT/hello" | grep -i "x-mock-peer-port"
  echo "--- upstream connections opened in total (mock view)"
  grep -ac "mock.conn.accepted" "$OUT/logs/mock.log"
} 2>&1 | tee "$OUT/7-pool.txt"

section "7b. pool is per service instance (one Arc across workers), not per worker"
start_gateway 18082 2 4 "$OUT/logs/gateway-4w.log"
{
  echo "--- 8 concurrent requests across 4 workers"
  for i in $(seq 1 8); do
    (curl -sS -D "/tmp/p$i.hdr" -o /dev/null "http://127.0.0.1:18082/sse?n=1&delay_ms=400" &)
  done
  sleep 1.5
  echo "workers that handled the burst:"
  grep -a "spike.done" "$OUT/logs/gateway-4w.log" | tail -8 | grep -o 'worker=ThreadId([0-9]*)' | sort | uniq -c
  echo "upstream ports used by the burst:"
  grep -a "mock.req.head" "$OUT/logs/mock.log" | grep "delay_ms=400" | tail -8 | grep -o 'peer_port=[0-9]*'
  echo "--- 8 sequential requests reuse exactly those ports"
  for _ in $(seq 1 8); do
    curl -sS -D - -o /dev/null "http://127.0.0.1:18082/pool" | grep -i "x-mock-peer-port"
  done
} 2>&1 | tee "$OUT/7b-pool-workers.txt"

##############################################################################
section "8. graceful shutdown (SIGTERM)"
section "8a. long SSE is truncated at the grace period"
start_gateway 18084 3 1 "$OUT/logs/gateway-grace.log"
{
  ( curl -sS -N "http://127.0.0.1:18084/sse?n=50&delay_ms=200" > /tmp/sse-grace.txt 2>/tmp/sse-grace.err & )
  sleep 0.6
  T0=$(python3 -c 'import time;print(time.time())')
  kill -TERM "$GW_PID"
  while kill -0 "$GW_PID" 2>/dev/null; do sleep 0.05; done
  T1=$(python3 -c 'import time;print(time.time())')
  echo "sigterm->exit seconds: $(python3 -c "print(round($T1-$T0,2))")"
  sleep 0.2
  echo "events received: $(grep -c '^data:' /tmp/sse-grace.txt)"
  echo "DONE received: $(grep -c '\[DONE\]' /tmp/sse-grace.txt)"
  echo "curl error: $(cat /tmp/sse-grace.err)"
} 2>&1 | tee -a "$OUT/8-graceful.txt"

section "8b. short SSE finishes inside the grace period; new connections are refused"
start_gateway 18084 3 1 "$OUT/logs/gateway-grace2.log"
{
  ( curl -sS -N "http://127.0.0.1:18084/sse?n=4&delay_ms=100" > /tmp/sse-short.txt 2>/tmp/sse-short.err & )
  sleep 0.15
  kill -TERM "$GW_PID"
  sleep 0.3
  echo -n "new request during grace: "
  curl -sS --max-time 1 "http://127.0.0.1:18084/hello" 2>&1 || echo " (connection refused)"
  while kill -0 "$GW_PID" 2>/dev/null; do sleep 0.05; done
  echo "short stream events: $(grep -c '^data:' /tmp/sse-short.txt) DONE: $(grep -c '\[DONE\]' /tmp/sse-short.txt)"
  echo "short stream error: $(cat /tmp/sse-short.err)"
} 2>&1 | tee -a "$OUT/8-graceful.txt"
grep -a "Graceful shutdown\|runtimes exited\|exiting now" "$OUT/logs/gateway-grace2.log" \
  | tail -6 | sed -E 's/^[^ ]+ +//' | tee -a "$OUT/8-graceful.txt"

section "8c. Pingora default grace period is 300s"
start_gateway 18084 default 1 "$OUT/logs/gateway-default-grace.log"
{
  kill -TERM "$GW_PID"
  sleep 8
  if kill -0 "$GW_PID" 2>/dev/null; then
    echo "still alive 8s after SIGTERM with zero in-flight requests"
  else
    echo "exited within 8s"
  fi
  grep -a "grace period" "$OUT/logs/gateway-default-grace.log" | sed -E 's/^[^ ]+ +//'
  stop_gateway
} 2>&1 | tee -a "$OUT/8-graceful.txt"

##############################################################################
section "9. WebSocket upgrade and gRPC over h2"
start_gateway "$GW_PORT" 3 1 "$OUT/logs/gateway-ws.log"
{
  echo "--- websocket echo"
  python3 scripts/ws_client.py 127.0.0.1:$GW_PORT /ws
  echo "--- gRPC shape: h2 downstream + h2 upstream + trailers"
  printf '\x00\x00\x00\x00\x05hello' > /tmp/grpc-req.bin
  curl -sS --http2-prior-knowledge -v \
    -H "content-type: application/grpc" -H "te: trailers" \
    --data-binary @/tmp/grpc-req.bin "http://127.0.0.1:$GW_PORT/grpc" \
    -o /tmp/grpc-resp.bin 2>/tmp/grpc-verbose.txt
  echo "exit=$?"
  echo "downstream response head + trailers seen by curl:"
  grep -E "^< " /tmp/grpc-verbose.txt
  echo "--- upstream h2 request as seen by the mock"
  grep -a "mock.h2.req" "$OUT/logs/mock.log" | tail -1 | sed -E 's/^[^ ]+ +//'
  echo "--- trailers delivered to the gateway filter"
  grep -a "spike.upstream.trailers" "$OUT/logs/gateway-ws.log" | tail -1 | sed -E 's/^[^ ]+ +//'
} 2>&1 | tee "$OUT/9-ws-grpc.txt"
grep -a "spike.done" "$OUT/logs/gateway-ws.log" | tail -2 | sed -E 's/^[^ ]+ +//' \
  | tee -a "$OUT/9-ws-grpc.txt"

##############################################################################
section "10. body consumed in a filter is not replayed upstream (known constraint)"
{
  echo "--- POST /peek: upstream_peer drains the body, then proxies"
  curl -sS --max-time 3 -X POST -H "Expect:" --data-binary @/tmp/spike-1mib.bin \
    "http://127.0.0.1:$GW_PORT/peek"
  echo "curl exit=$? (28 = timed out because the upstream never got the body)"
  grep -a "spike.req.peeked" "$OUT/logs/gateway-ws.log" | sed -E 's/^[^ ]+ +//'
  grep -a "spike.upstream.request" "$OUT/logs/gateway-ws.log" | tail -1 | sed -E 's/^[^ ]+ +//'
  sleep 0.5
  grep -a "mock.req.head\|mock.conn.error" "$OUT/logs/mock.log" | tail -2 | sed -E 's/^[^ ]+ +//'
} 2>&1 | tee "$OUT/10-body-consumed.txt"

section "duplicate dependency versions relevant to a later etcd client"
cargo tree -d 2>&1 | grep -E "^(h2|hyper|hyper-util|tokio|tokio-rustls|rustls|openssl|ring|tonic|prost) v" \
  | sort -u | tee "$OUT/cargo-tree-d.txt"

section "done"
log "evidence written to $OUT"
