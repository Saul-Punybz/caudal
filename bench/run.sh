#!/usr/bin/env bash
# Caudal vs MediaMTX, one command:  bench/run.sh [bench.py run options]
#   e.g. bench/run.sh --reps 1 --levels 1,100 --protos hls
# Builds Caudal and the load client (release), downloads and verifies
# MediaMTX, renders the source, runs every scenario, prints the tables.
# Needs: cargo, ffmpeg/ffprobe, python3, curl. Run on an otherwise idle
# machine; results land in bench/results/<timestamp>.jsonl (+ .log).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(dirname "$HERE")"

ulimit -n 65536 2>/dev/null || ulimit -n "$(ulimit -Hn)"

(cd "$ROOT" && cargo build --release -p caudal)
(cd "$HERE/client" && cargo build --release)
"$HERE/fetch-mediamtx.sh" >/dev/null
python3 "$HERE/bench.py" prepare

leftovers="$(pgrep -f 'target/release/caudal |mediamtx-v|caudal-bench-client' || true)"
if [ -n "$leftovers" ]; then
  echo "refusing to run: a caudal/mediamtx/bench process is already running: $leftovers" >&2
  exit 1
fi

results="$(python3 "$HERE/bench.py" run "$@")"
python3 "$HERE/bench.py" report "$results" | tee "${results%.jsonl}.md"
