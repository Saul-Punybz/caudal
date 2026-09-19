#!/usr/bin/env python3
"""Soak test: Caudal alone, for hours, on GitHub's Linux runners.

    python3 bench/soak.py run [--minutes 120] [--churn-minutes 15]
                               [--sample-s 30] [--streams 3] [--viewers 10]
    python3 bench/soak.py verdict RESULTS.csv [RESULTS.meta.json]

`run` starts Caudal (bench/caudal-soak.toml: recording on, everything else
loopback), 2-3 RTMP publishers, and long-lived viewers on LL-HLS, RTSP and
WHEP (bench/client, one process per stream x protocol, held open for
~the whole run). Every `--churn-minutes` it restarts one publisher
(disconnect + republish) and starts a short-lived extra batch of viewers
(join, watch, leave); once each, early in the run, it does one
`POST /api/v1/config/reload` and one clip. Every `--sample-s` it samples the
server's RSS/fd/thread/CPU (from /proc) and /metrics (streams, viewers,
bytes) plus the viewers' own error/timeout counts (bench/client
`--report-interval`, added for this) to a CSV.

`verdict` reads that CSV (+ the `.meta.json` `run` wrote next to it), checks
the pass/fail rules below, plots a PNG next to the CSV, prints a Markdown
summary to stdout, and exits non-zero on failure:
  - after a 10-minute warm-up: RSS slope < 1 MB/h and total RSS growth < 10%
  - fd count and thread count flat (no sustained per-hour growth)
  - no viewer client mode ended with a stuck viewer (kept_up < viewers)
  - no "panicked" in the server log
  - all configured streams present in the final /metrics sample, every
    publisher still running, and (if attempted) the reload and the clip
    both succeeded

Reuses bench/bench.py as a module: prepare(), SRC, machine(), ps(), spawn(),
stop(). bench.py's own signal handling lives under its `__main__` guard, so
importing it here does not install anything; this file installs its own.
"""

import csv
import json
import os
import re
import resource
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import bench  # noqa: E402  (bench/bench.py; prepare/SRC/machine/ps/spawn/stop)

RESULTS = bench.RESULTS
SOAK_TOML = os.path.join(HERE, "caudal-soak.toml")
BASE = "http://127.0.0.1:8080"

PROTOCOLS = ("hls", "rtsp", "whep")


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, file=sys.stderr, flush=True)


# ---------------------------------------------------------------- proc / http

def fd_count(pid):
    try:
        return len(os.listdir(f"/proc/{pid}/fd"))
    except OSError:
        return None


def thread_count(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("Threads:"):
                    return int(line.split()[1])
    except OSError:
        return None
    return None


def http_get(path, timeout=5):
    with urllib.request.urlopen(BASE + path, timeout=timeout) as r:
        return r.status, r.read().decode()


def http_post(path, body=b"", timeout=15, content_type=None):
    headers = {"Content-Type": content_type} if content_type else {}
    req = urllib.request.Request(BASE + path, method="POST", data=body, headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, r.read()


def fetch_metrics():
    try:
        _, body = http_get("/metrics", timeout=5)
    except (urllib.error.URLError, OSError, TimeoutError):
        return None
    m = re.search(r"^caudal_streams (\d+)", body, re.M)
    viewers = sum(int(v) for v in re.findall(r'^caudal_viewers\{[^}]*\} (\d+)', body, re.M))
    bytes_in = sum(int(v) for v in re.findall(r'^caudal_bytes_in_total\{[^}]*\} (\d+)', body, re.M))
    return {"streams": int(m.group(1)) if m else None, "viewers": viewers, "bytes_in": bytes_in}


def hls_url(stream):
    return f"{BASE}/hls/{stream}/master.m3u8"


def rtsp_url(stream):
    return f"rtsp://127.0.0.1:8554/{stream}"


def whep_url(stream):
    return f"{BASE}/whep/{stream}"


def rtmp_url(stream):
    return f"rtmp://127.0.0.1:1935/live/{stream}"


def wait_playlist(stream, timeout=20):
    """Waits until `stream` is live on LL-HLS (media playlist has parts).
    Same idea as bench.wait_playlist, but for an arbitrary stream name
    instead of the fixed "bench" bench.py compares Caudal and MediaMTX
    with."""
    end = time.time() + timeout
    url = hls_url(stream)
    while time.time() < end:
        try:
            body = urllib.request.urlopen(url, timeout=1).read().decode()
            if "#EXT-X-STREAM-INF" in body:
                line = [ln for ln in body.splitlines() if ln and not ln.startswith("#")][0]
                body = urllib.request.urlopen(urllib.request.urljoin(url, line), timeout=2).read().decode()
            if body.count("#EXT-X-PART:") >= 3 and "#EXTINF" in body:
                return True
        except Exception:
            pass
        time.sleep(0.25)
    return False


# ---------------------------------------------------------------- workload

def spawn_publisher(stream, logf):
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-re", "-stream_loop", "-1",
           "-i", bench.SRC, "-c", "copy", "-f", "flv", rtmp_url(stream)]
    return bench.spawn(cmd, stdout=logf, stderr=subprocess.STDOUT)


def spawn_viewer_client(proto, stream, n, warmup, duration, rates, report_interval, out_path):
    url = {"hls": hls_url, "rtsp": rtsp_url, "whep": whep_url}[proto](stream)
    expect = rates["video"] + rates["audio"] if proto == "rtsp" else rates["video"]
    ramp_ms = min(10000, max(0, n * 200))
    cmd = [bench.CLIENT_BIN, proto, url, "--viewers", str(n), "--warmup", str(warmup),
           "--duration", str(duration), "--ramp-ms", str(ramp_ms), "--expect-mbps", f"{expect:.3f}"]
    if report_interval:
        cmd += ["--report-interval", str(report_interval)]
    f = open(out_path, "w")
    return bench.spawn(cmd, stdout=f, stderr=subprocess.STDOUT), f


TICK_OFFSETS = {}


def poll_ticks(key, path):
    """New `{"kind":"tick",...}` lines a persistent client wrote since the
    last poll of this key: summed errors/timeouts/reconnects for the CSV."""
    off = TICK_OFFSETS.get(key, 0)
    try:
        with open(path) as f:
            f.seek(off)
            new = f.read()
            TICK_OFFSETS[key] = f.tell()
    except OSError:
        return 0, 0, 0
    errors = timeouts = reconnected = 0
    for line in new.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("kind") == "tick":
            errors += rec.get("errors", 0)
            timeouts += rec.get("timeouts", 0)
            reconnected += rec.get("reconnected", 0)
    return errors, timeouts, reconnected


def last_json_line(path):
    try:
        lines = [ln for ln in open(path).read().splitlines() if ln.strip().startswith("{")]
    except OSError:
        return None
    for line in reversed(lines):
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("kind") != "tick":
            return rec
    return None


def do_reload():
    try:
        status, body = http_post("/api/v1/config/reload")
        return {"ok": status == 200, "status": status, "body": json.loads(body.decode())}
    except Exception as e:
        return {"ok": False, "error": str(e)}


def try_clip(stream):
    """None while no recording is ready yet; else the clip result (ok or
    not) after one attempt. Called every sample until it returns non-None
    or the soak gives up (see run())."""
    try:
        _, body = http_get("/api/v1/recordings", timeout=5)
        recs = json.loads(body)
    except Exception as e:
        return {"ok": False, "error": f"list recordings: {e}"}
    cand = next((m for m in recs if m.get("stream") == stream and m.get("duration_ms", 0) >= 5000), None)
    if cand is None:
        return None
    to_ms = min(5000, cand["duration_ms"])
    payload = json.dumps({"stream": stream, "id": cand["id"], "from_ms": 0, "to_ms": to_ms}).encode()
    t0 = time.monotonic()
    try:
        status, body = http_post("/api/v1/clips", body=payload, timeout=30, content_type="application/json")
        return {"ok": status == 200 and len(body) > 0, "status": status, "bytes": len(body),
                "recording_id": cand["id"], "to_ms": to_ms, "elapsed_s": round(time.monotonic() - t0, 2)}
    except Exception as e:
        return {"ok": False, "error": str(e), "recording_id": cand["id"]}


# ---------------------------------------------------------------- run

def run(argv):
    minutes, churn_minutes, sample_s, streams_n, viewers_n = 120, 15, 30, 3, 10
    it = iter(argv)
    for a in it:
        if a == "--minutes":
            minutes = int(next(it))
        elif a == "--churn-minutes":
            churn_minutes = int(next(it))
        elif a == "--sample-s":
            sample_s = int(next(it))
        elif a == "--streams":
            streams_n = int(next(it))
        elif a == "--viewers":
            viewers_n = int(next(it))
        else:
            sys.exit(f"unknown option {a}")

    for binary in (bench.CAUDAL_BIN, bench.CLIENT_BIN):
        if not os.path.exists(binary):
            sys.exit(f"missing {binary}; build it first (see .github/workflows/soak.yml)")
    try:
        resource.setrlimit(resource.RLIMIT_NOFILE, (65536, resource.getrlimit(resource.RLIMIT_NOFILE)[1]))
    except (ValueError, OSError):
        pass

    bench.prepare()
    os.makedirs(RESULTS, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    csv_path = os.path.join(RESULTS, f"soak-{stamp}.csv")
    meta_path = os.path.join(RESULTS, f"soak-{stamp}.meta.json")
    server_log = os.path.join(RESULTS, f"soak-{stamp}.server.log")
    stream_names = [f"soak{i}" for i in range(streams_n)]

    total_s = minutes * 60
    client_warmup, client_tail = 20, 15
    client_duration = max(30, total_s - client_warmup - client_tail)

    logf = open(server_log, "w")
    os.environ["CAUDAL_CONFIG"] = SOAK_TOML
    log("starting caudal:", SOAK_TOML)
    srv = bench.start_server("caudal", logf)
    rates = bench.src_rates()

    publishers = {}
    for name in stream_names:
        publishers[name] = spawn_publisher(name, logf)
    ready = {name: wait_playlist(name, timeout=30) for name in stream_names}
    for name, ok in ready.items():
        log(f"stream {name} live: {ok}")

    client_procs = {}  # key "<stream>/<proto>" -> (Popen, file, out_path)
    for name in stream_names:
        for proto in PROTOCOLS:
            key = f"{name}/{proto}"
            out_path = os.path.join(RESULTS, f"soak-{stamp}.{key.replace('/', '-')}.out")
            p, f = spawn_viewer_client(proto, name, viewers_n, client_warmup, client_duration, rates, sample_s,
                                        out_path)
            client_procs[key] = (p, f, out_path)
    log(f"{len(client_procs)} viewer client processes started "
        f"({streams_n} streams x {len(PROTOCOLS)} protocols x {viewers_n} viewers)")

    churn_events = []
    reload_result = None
    clip_result = None
    clip_deadline_s = 300  # give up trying after 5 minutes

    rows = []
    fieldnames = ["t_s", "ts", "rss_mb", "fd_count", "threads", "cpu_s",
                  "streams", "viewers", "bytes_in", "client_errors", "client_timeouts", "client_reconnected"]
    with open(csv_path, "w", newline="") as cf:
        writer = csv.DictWriter(cf, fieldnames=fieldnames)
        writer.writeheader()

        loop_start = time.monotonic()
        cpu0 = (bench.ps(srv.pid) or (0.0, 0.0))[1]
        next_sample = loop_start
        next_churn = churn_minutes * 60
        cycle = 0
        while True:
            elapsed = time.monotonic() - loop_start
            if elapsed >= total_s:
                break
            if next_sample <= time.monotonic():
                r = bench.ps(srv.pid)
                rss = r[0] if r else None
                cpu = r[1] if r else cpu0
                m = fetch_metrics() or {}
                errs = tos = recs_ = 0
                for key, (_, _, out_path) in client_procs.items():
                    e, t, rc = poll_ticks(key, out_path)
                    errs += e
                    tos += t
                    recs_ += rc
                row = {
                    "t_s": round(elapsed, 1), "ts": time.strftime("%Y-%m-%dT%H:%M:%S"),
                    "rss_mb": rss, "fd_count": fd_count(srv.pid), "threads": thread_count(srv.pid),
                    "cpu_s": round(cpu, 1), "streams": m.get("streams"), "viewers": m.get("viewers"),
                    "bytes_in": m.get("bytes_in"), "client_errors": errs, "client_timeouts": tos,
                    "client_reconnected": recs_,
                }
                writer.writerow(row)
                cf.flush()
                rows.append(row)
                log(f"t={row['t_s']:.0f}s rss={rss} fd={row['fd_count']} threads={row['threads']} "
                    f"streams={row['streams']} viewers={row['viewers']} errs={errs} to={tos}")
                next_sample += sample_s

            if reload_result is None and elapsed >= 90:
                reload_result = do_reload()
                log("config reload:", reload_result)
            if clip_result is None and elapsed >= 60:
                r = try_clip(stream_names[0])
                if r is not None:
                    clip_result = r
                    log("clip:", clip_result)
                elif elapsed >= clip_deadline_s:
                    clip_result = {"ok": False, "error": "no recording with >= 5s reached the deadline"}
                    log("clip: gave up,", clip_result)

            if elapsed >= next_churn:
                cycle += 1
                target = stream_names[cycle % len(stream_names)]
                log(f"churn #{cycle}: restarting publisher {target}")
                bench.stop(publishers[target], grace=2.0)
                time.sleep(1.0)
                publishers[target] = spawn_publisher(target, logf)
                ok = wait_playlist(target, timeout=20)
                churn_events.append({"t_s": round(elapsed, 1), "action": "publisher_restart",
                                      "stream": target, "reconnected": ok})
                proto = PROTOCOLS[cycle % len(PROTOCOLS)]
                extra = stream_names[(cycle + 1) % len(stream_names)]
                out_path = os.path.join(RESULTS, f"soak-{stamp}.churn-{cycle}.out")
                dur = max(20, min(60, churn_minutes * 60 - 10))
                # Fire-and-forget: this batch runs its own course and exits;
                # only the file handle needs closing on our side (the child
                # keeps its own, dup'd, copy of the fd).
                _, extra_f = spawn_viewer_client(proto, extra, max(2, viewers_n // 2), 5, dur, rates, 0, out_path)
                extra_f.close()
                churn_events.append({"t_s": round(elapsed, 1), "action": "extra_viewers",
                                      "stream": extra, "proto": proto, "duration_s": dur})
                next_churn += churn_minutes * 60

            time.sleep(min(1.0, max(0.0, next_sample - time.monotonic())))

    log("soak loop done; waiting for persistent viewer clients to exit")
    client_summaries = {}
    for key, (p, f, out_path) in client_procs.items():
        try:
            p.wait(client_tail + 30)
        except subprocess.TimeoutExpired:
            bench.stop(p, 5.0)
        f.close()
        client_summaries[key] = last_json_line(out_path)

    publisher_alive = {name: (p.poll() is None) for name, p in publishers.items()}
    final_metrics = fetch_metrics()
    panicked = False
    try:
        panicked = "panicked" in open(server_log, errors="replace").read()
    except OSError:
        pass

    for p in publishers.values():
        bench.stop(p)
    bench.stop(srv)

    meta = {
        "stamp": stamp, "minutes": minutes, "churn_minutes": churn_minutes, "sample_s": sample_s,
        "stream_count": streams_n, "viewers_per_stream": viewers_n, "protocols": list(PROTOCOLS),
        "streams": stream_names, "server_log": server_log, "csv": csv_path,
        "machine": bench.machine(), "src_mbps": rates, "record_enabled": True,
        "warmup_s": 600, "reload_result": reload_result, "clip_result": clip_result,
        "churn_events": churn_events, "client_summaries": client_summaries,
        "publisher_alive_at_end": publisher_alive, "final_metrics": final_metrics,
        "server_log_has_panic": panicked,
    }
    with open(meta_path, "w") as f:
        json.dump(meta, f, indent=2)
    log("results:", csv_path)
    log("meta:", meta_path)
    print(csv_path)


# ---------------------------------------------------------------- verdict

def lstsq_slope(xs, ys):
    """Least-squares slope of ys against xs (Nones dropped pairwise)."""
    pts = [(x, y) for x, y in zip(xs, ys) if x is not None and y is not None]
    if len(pts) < 2:
        return None
    n = len(pts)
    sx = sum(p[0] for p in pts)
    sy = sum(p[1] for p in pts)
    sxx = sum(p[0] * p[0] for p in pts)
    sxy = sum(p[0] * p[1] for p in pts)
    denom = n * sxx - sx * sx
    if denom == 0:
        return None
    return (n * sxy - sx * sy) / denom


def read_csv(path):
    with open(path, newline="") as f:
        rows = list(csv.DictReader(f))
    out = []
    for r in rows:
        def num(k):
            v = r.get(k)
            if v in (None, "", "None"):
                return None
            try:
                return float(v)
            except ValueError:
                return None
        out.append({"t_s": num("t_s"), "ts": r.get("ts"), "rss_mb": num("rss_mb"), "fd_count": num("fd_count"),
                     "threads": num("threads"), "cpu_s": num("cpu_s"), "streams": num("streams"),
                     "viewers": num("viewers"), "bytes_in": num("bytes_in"), "client_errors": num("client_errors"),
                     "client_timeouts": num("client_timeouts"), "client_reconnected": num("client_reconnected")})
    return out


def plot(rows, png_path):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    t_h = [r["t_s"] / 3600.0 if r["t_s"] is not None else None for r in rows]
    fig, axes = plt.subplots(2, 2, figsize=(11, 7))

    ax = axes[0][0]
    ax.plot(t_h, [r["rss_mb"] for r in rows], color="tab:blue")
    ax.set_title("Server RSS (MB)")
    ax.set_xlabel("hours")

    ax = axes[0][1]
    ax.plot(t_h, [r["fd_count"] for r in rows], label="fds", color="tab:orange")
    ax.plot(t_h, [r["threads"] for r in rows], label="threads", color="tab:green")
    ax.set_title("fds / threads")
    ax.set_xlabel("hours")
    ax.legend()

    ax = axes[1][0]
    ax.plot(t_h, [r["cpu_s"] for r in rows], color="tab:red")
    ax.set_title("Server CPU, cumulative seconds")
    ax.set_xlabel("hours")

    ax = axes[1][1]
    ax.plot(t_h, [r["viewers"] for r in rows], label="viewers (/metrics)", color="tab:purple")
    ax.plot(t_h, [r["streams"] for r in rows], label="streams (/metrics)", color="tab:brown")
    ax.set_title("streams / viewers")
    ax.set_xlabel("hours")
    ax.legend()

    fig.tight_layout()
    fig.savefig(png_path, dpi=110)


def verdict(csv_path, meta_path=None):
    if meta_path is None:
        meta_path = csv_path.replace(".csv", ".meta.json")
    rows = read_csv(csv_path)
    meta = json.load(open(meta_path)) if os.path.exists(meta_path) else {}
    warmup_s = meta.get("warmup_s", 600)
    fails = []
    notes = []

    post = [r for r in rows if r["t_s"] is not None and r["t_s"] >= warmup_s and r["rss_mb"] is not None]
    if len(post) < 3:
        fails.append(f"only {len(post)} post-warm-up samples with an RSS reading; can't judge growth")
    else:
        t = [r["t_s"] / 3600.0 for r in post]
        rss = [r["rss_mb"] for r in post]
        slope = lstsq_slope(t, rss)
        growth_pct = (rss[-1] - rss[0]) / rss[0] * 100 if rss[0] else None
        if slope is None or slope >= 1.0:
            fails.append(f"RSS slope {slope:.2f} MB/h >= 1.0 MB/h" if slope is not None
                         else "RSS slope: not enough data")
        if growth_pct is None or growth_pct >= 10.0:
            fails.append(f"RSS growth {growth_pct:.1f}% over the post-warm-up window >= 10%"
                          if growth_pct is not None else "RSS growth: not enough data")
        fd = [r["fd_count"] for r in post]
        fd_slope = lstsq_slope(t, fd) if all(v is not None for v in fd) else None
        if fd_slope is not None and fd_slope >= 5.0:
            fails.append(f"fd count slope {fd_slope:.1f}/h, not flat")
        th = [r["threads"] for r in post]
        th_slope = lstsq_slope(t, th) if all(v is not None for v in th) else None
        if th_slope is not None and th_slope >= 2.0:
            fails.append(f"thread count slope {th_slope:.1f}/h, not flat")
        if slope is not None and growth_pct is not None:
            notes.append(f"RSS slope {slope:.3f} MB/h, growth {growth_pct:.1f}%")

    log_path = meta.get("server_log")
    if log_path and os.path.exists(log_path):
        if "panicked" in open(log_path, errors="replace").read():
            fails.append("server log contains 'panicked'")
    elif meta.get("server_log_has_panic"):
        fails.append("server log contains 'panicked'")

    for key, summary in (meta.get("client_summaries") or {}).items():
        if summary is None:
            fails.append(f"{key}: viewer client produced no final summary")
            continue
        viewers = summary.get("viewers")
        kept = summary.get("kept_up")
        if kept is not None and viewers is not None and kept != viewers:
            fails.append(f"{key}: kept_up {kept}/{viewers} (stuck viewer)")

    if rows:
        last = rows[-1]
        expected = meta.get("stream_count")
        if expected is not None and last["streams"] != expected:
            fails.append(f"final caudal_streams={last['streams']} != expected {expected}")
    for name, alive in (meta.get("publisher_alive_at_end") or {}).items():
        if not alive:
            fails.append(f"publisher {name} was not running at the end")

    reload_result = meta.get("reload_result")
    if reload_result is None or not reload_result.get("ok"):
        fails.append(f"config reload did not succeed: {reload_result}")
    clip_result = meta.get("clip_result")
    if clip_result is None or not clip_result.get("ok"):
        fails.append(f"clip did not succeed: {clip_result}")

    png_path = csv_path.replace(".csv", ".png")
    try:
        plot(rows, png_path)
    except Exception as e:
        notes.append(f"plot failed: {e}")

    out = []
    p = out.append
    verdict_str = "PASS" if not fails else "FAIL"
    p(f"### Soak test: {verdict_str}")
    p("")
    if meta:
        p(f"{meta.get('minutes')} min, {meta.get('stream_count')} streams x {len(meta.get('protocols', []))} "
          f"protocols x {meta.get('viewers_per_stream')} viewers, churn every {meta.get('churn_minutes')} min, "
          f"sampled every {meta.get('sample_s')} s.  ")
        p(f"Machine: {json.dumps(meta.get('machine'))}  ")
    p(f"CSV: `{os.path.relpath(csv_path, bench.ROOT)}`, plot: `{os.path.relpath(png_path, bench.ROOT)}`")
    p("")
    if fails:
        p("**Failures:**")
        for f in fails:
            p(f"- {f}")
        p("")
    if notes:
        p("Notes: " + "; ".join(notes))
        p("")
    def n(v, digits=0):
        return "?" if v is None else f"{v:.{digits}f}"

    if rows:
        last = rows[-1]
        p("| Metric | Value |")
        p("|---|---|")
        p(f"| Samples | {len(rows)} |")
        p(f"| Final RSS | {n(last['rss_mb'], 1)} MB |")
        p(f"| Final fd count | {n(last['fd_count'])} |")
        p(f"| Final threads | {n(last['threads'])} |")
        p(f"| Final streams / viewers (metrics) | {n(last['streams'])} / {n(last['viewers'])} |")
        p(f"| Total client errors / timeouts | "
          f"{sum(r['client_errors'] or 0 for r in rows):.0f} / {sum(r['client_timeouts'] or 0 for r in rows):.0f} |")
        p("")
    if meta.get("client_summaries"):
        p("| Stream/proto | Viewers | Kept up | Errors | Timeouts | Reconnected |")
        p("|---|---|---|---|---|---|")
        for key, s in meta["client_summaries"].items():
            if s is None:
                p(f"| {key} | - | NO SUMMARY | | | |")
                continue
            p(f"| {key} | {s.get('viewers')} | {s.get('kept_up')} | {s.get('errors')} | "
              f"{s.get('timeouts')} | {s.get('reconnected')} |")
        p("")
    p(f"Config reload: `{json.dumps(meta.get('reload_result'))}`  ")
    p(f"Clip: `{json.dumps(meta.get('clip_result'))}`  ")
    if meta.get("churn_events"):
        p("")
        p("Churn events: " + "; ".join(f"t={e['t_s']:.0f}s {e['action']} {e.get('stream', '')}"
                                        for e in meta["churn_events"]))
    print("\n".join(out))
    return 0 if not fails else 1


# ---------------------------------------------------------------- main

def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    cmd = sys.argv[1]
    if cmd == "prepare":
        bench.prepare()
    elif cmd == "run":
        run(sys.argv[2:])
    elif cmd == "verdict":
        if len(sys.argv) < 3:
            sys.exit(__doc__)
        rc = verdict(sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None)
        sys.exit(rc)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: (bench.cleanup(), sys.exit(1)))
    try:
        main()
    finally:
        bench.cleanup()
