#!/usr/bin/env bash
# Where Caudal's memory goes with one live stream:  bench/heap.sh [SECONDS]
# Builds Caudal with the `dhat-heap` feature (every allocation recorded with
# its backtrace), publishes the bench source for SECONDS (default 70, past
# the default 50 s live buffer), stops the server with SIGTERM and prints
# the allocation sites holding the most bytes at the heap's peak.
# Slow and heavy: meant for the Linux bench runner (bench.yml, input heap).
# Output: bench/results/dhat-heap.json and dhat-top.txt.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(dirname "$HERE")"
SECS="${1:-70}"
CONFIG="${CAUDAL_CONFIG:-$HERE/caudal.toml}"

# Symbols for the backtraces; codegen is unchanged (same opt-level, LTO).
export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only CARGO_PROFILE_RELEASE_STRIP=none
(cd "$ROOT" && cargo build --release -p caudal --features dhat-heap)
python3 "$HERE/bench.py" prepare
mkdir -p "$HERE/results"
cd "$HERE/results"
rm -f dhat-heap.json

"$ROOT/target/release/caudal" --config "$CONFIG" > heap-server.log 2>&1 &
srv=$!
pub=
trap 'kill $pub $srv 2>/dev/null || true' EXIT
for _ in $(seq 60); do curl -sf http://127.0.0.1:8080/healthz >/dev/null && break; sleep 0.5; done
ffmpeg -hide_banner -loglevel error -re -stream_loop -1 -i "$HERE/.cache/src-1080p30-6M-120s.mp4" \
  -c copy -f flv rtmp://127.0.0.1:1935/live/bench > heap-publisher.log 2>&1 &
pub=$!
sleep "$SECS"
ps -o rss= -p "$srv" | awk '{printf "server RSS before stop (dhat allocator): %.1f MB\n", $1/1024}' | tee dhat-top.txt
curl -s http://127.0.0.1:8080/api/v1/streams | head -c 2000 >> dhat-top.txt || true
echo >> dhat-top.txt
kill "$pub"; wait "$pub" 2>/dev/null || true
kill -TERM "$srv"; wait "$srv" || true
pub= srv=
python3 "$HERE/dhat_top.py" dhat-heap.json | tee -a dhat-top.txt
