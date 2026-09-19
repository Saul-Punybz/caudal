#!/usr/bin/env python3
"""Caudal vs MediaMTX, side by side on one machine. Standard library only.

    python3 bench/bench.py prepare          # source media (needs ffmpeg)
    python3 bench/bench.py run [--reps 3] [--only idle,publish,fanout,latency]
                                  [--levels 1,100,1000] [--protos hls,rtsp,whep]
    python3 bench/bench.py report RESULTS.jsonl

`bench/run.sh` builds both sides and calls prepare + run + report.
One server runs at a time. Every scenario starts a fresh server (and a
fresh publisher), so no run inherits another one's memory or state.
"""

import json
import os
import platform
import resource
import signal
import socket
import statistics
import subprocess
import sys
import threading
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CACHE = os.path.join(HERE, ".cache")
RESULTS = os.path.join(HERE, "results")
MTX_VERSION = os.environ.get("MEDIAMTX_VERSION", "v1.21.0")
MTX_BIN = os.path.join(CACHE, f"mediamtx-{MTX_VERSION}", "mediamtx")
CAUDAL_BIN = os.path.join(ROOT, "target", "release", "caudal")
CLIENT_BIN = os.path.join(HERE, "client", "target", "release", "caudal-bench-client")
SRC = os.path.join(CACHE, "src-1080p30-6M-120s.mp4")

# One definition of the test signal, used for the file (throughput runs) and
# the live encode (latency runs). The top 40 rows carry a 32-bit stamp:
# wall-clock ms (mod 2^32) at the moment the frame leaves the filter graph.
FILTER = (
    "[0:v]settb=1/1000,setpts=RTCTIME/1000,split[a][b];"
    "[b]crop=1920:40:0:0,geq=lum='if(mod(floor(mod(round(T*1000),4294967296)"
    "/pow(2,31-floor(X/60))),2),235,16)':cb=128:cr=128[s];"
    "[a]noise=alls=12:allf=t[n];[n][s]overlay=0:0,setpts=N/(30*TB)[v]"
)
ENCODE = [
    "-map", "[v]", "-map", "1:a",
    "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency", "-profile:v", "high",
    "-b:v", "6M", "-maxrate", "6M", "-bufsize", "3M", "-g", "60", "-keyint_min", "60",
    "-sc_threshold", "0", "-pix_fmt", "yuv420p",
    "-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2",
]
LAVFI = ["-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=30",
         "-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000"]

SERVERS = {
    "caudal": {
        # CAUDAL_CONFIG: e.g. bench/caudal-14s.toml for the buffer-size sensitivity run.
        "cmd": lambda: [CAUDAL_BIN, "--config", os.environ.get("CAUDAL_CONFIG", os.path.join(HERE, "caudal.toml"))],
        "ready": "http://127.0.0.1:8080/healthz",
        "rtmp": "rtmp://127.0.0.1:1935/live/bench",
        "hls": "http://127.0.0.1:8080/hls/bench/master.m3u8",
        "rtsp": "rtsp://127.0.0.1:8554/bench",
        "whep": "http://127.0.0.1:8080/whep/bench",
    },
    "mediamtx": {
        "cmd": lambda: [MTX_BIN, os.path.join(HERE, "mediamtx.yml")],
        "ready": ("127.0.0.1", 8888),
        "rtmp": "rtmp://127.0.0.1:1935/live/bench",
        "hls": "http://127.0.0.1:8888/live/bench/index.m3u8",
        "rtsp": "rtsp://127.0.0.1:8554/live/bench",
        "whep": "http://127.0.0.1:8889/live/bench/whep",
    },
}

CHILDREN = []  # every process we start, killed on exit


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, file=sys.stderr, flush=True)


def spawn(cmd, **kw):
    p = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, start_new_session=True, **kw)
    CHILDREN.append(p)
    return p


def stop(p, grace=5.0):
    if p is None or p.poll() is not None:
        return
    try:
        os.killpg(p.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        p.wait(grace)
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGKILL)
        p.wait()


def cleanup(*_):
    for p in CHILDREN:
        stop(p, 2.0)


# ---------------------------------------------------------------- sampling

def cputime(s):
    """ps TIME ([[dd-]hh:]mm:ss.cc) -> seconds."""
    days = 0
    if "-" in s:
        d, s = s.split("-", 1)
        days = int(d)
    parts = [float(x) for x in s.split(":")]
    secs = 0.0
    for x in parts:
        secs = secs * 60 + x
    return days * 86400 + secs


LINUX = sys.platform.startswith("linux")


def ps(pid):
    """(rss_mb, cpu_seconds) or None."""
    if LINUX:
        # /proc, not ps: Linux ps prints TIME in whole seconds, too coarse
        # for a 28 s window.
        try:
            with open(f"/proc/{pid}/stat") as f:
                fields = f.read().rsplit(")", 1)[1].split()
            ticks = os.sysconf("SC_CLK_TCK")
            cpu = (int(fields[11]) + int(fields[12])) / ticks
            with open(f"/proc/{pid}/status") as f:
                rss_kb = next(int(l.split()[1]) for l in f if l.startswith("VmRSS:"))
            return rss_kb / 1024.0, cpu
        except (OSError, StopIteration, IndexError, ValueError):
            return None
    out = subprocess.run(["ps", "-o", "rss=,time=", "-p", str(pid)], capture_output=True, text=True).stdout.split()
    if len(out) < 2:
        return None
    return int(out[0]) / 1024.0, cputime(out[1])


def other_cpu(ours):
    out = subprocess.run(["ps", "-Ao", "pid=,pcpu="], capture_output=True, text=True).stdout
    tot = 0.0
    for line in out.splitlines():
        f = line.split()
        if len(f) == 2 and int(f[0]) not in ours and int(f[0]) != os.getpid():
            tot += float(f[1])
    return tot


class Window:
    """CPU% (from cumulative CPU time) and RSS samples (1 Hz) for some pids."""

    def __init__(self, pids):
        self.pids = pids
        self.rss = {k: [] for k in pids}
        self.stop_ev = threading.Event()

    def __enter__(self):
        self.t0 = time.monotonic()
        self.c0 = {k: (ps(p) or (0, 0))[1] for k, p in self.pids.items()}
        self.th = threading.Thread(target=self._loop, daemon=True)
        self.th.start()
        return self

    def _loop(self):
        self.others = []
        while not self.stop_ev.wait(1.0):
            for k, p in self.pids.items():
                r = ps(p)
                if r:
                    self.rss[k].append(r[0])
            self.others.append(other_cpu(set(self.pids.values())))

    def __exit__(self, *_):
        self.stop_ev.set()
        self.th.join()
        dt = time.monotonic() - self.t0
        self.result = {}
        for k, p in self.pids.items():
            r = ps(p)
            c1 = r[1] if r else self.c0[k]
            if r:
                self.rss[k].append(r[0])
            s = self.rss[k] or [0.0]
            self.result[k] = {
                "cpu_pct": round(100.0 * (c1 - self.c0[k]) / dt, 1),
                "rss_mb_median": round(statistics.median(s), 1),
                "rss_mb_max": round(max(s), 1),
                "window_s": round(dt, 1),
            }
        # Everything else on the machine (ps %cpu, a decaying average), so a
        # busy neighbour shows up next to the numbers it may have skewed.
        self.result["other_cpu_pct_median"] = round(statistics.median(self.others), 1) if self.others else None


def kernel_counters():
    """Machine-wide limits a single-box fan-out can hit: mbuf allocations the
    kernel refused, and UDP datagrams dropped on full socket buffers."""
    import re
    if LINUX:
        # /proc/net/snmp "Udp:" header + values; buffer errors = drops on
        # full socket buffers (receive and send). No mbuf counter on Linux.
        try:
            lines = [l.split() for l in open("/proc/net/snmp") if l.startswith("Udp:")]
            row = dict(zip(lines[0][1:], (int(x) for x in lines[1][1:])))
            return {"mbuf_denied": None,
                    "udp_full_drops": row.get("RcvbufErrors", 0) + row.get("SndbufErrors", 0)}
        except (OSError, IndexError, ValueError):
            return {"mbuf_denied": None, "udp_full_drops": None}
    m = subprocess.run(["netstat", "-m"], capture_output=True, text=True).stdout
    u = subprocess.run(["netstat", "-s", "-p", "udp"], capture_output=True, text=True).stdout
    denied = re.search(r"(\d+) requests for memory denied", m)
    full = re.search(r"(\d+) dropped due to full socket buffers", u)
    return {"mbuf_denied": int(denied.group(1)) if denied else None,
            "udp_full_drops": int(full.group(1)) if full else None}


# ---------------------------------------------------------------- servers

def wait_ready(server, timeout=20):
    target = SERVERS[server]["ready"]
    end = time.time() + timeout
    while time.time() < end:
        try:
            if isinstance(target, tuple):
                socket.create_connection(target, 0.5).close()
            else:
                urllib.request.urlopen(target, timeout=0.5).read()
            return True
        except Exception:
            time.sleep(0.1)
    return False


def start_server(server, logf):
    p = spawn(SERVERS[server]["cmd"](), stdout=logf, stderr=subprocess.STDOUT, cwd=CACHE)
    if not wait_ready(server):
        stop(p)
        raise RuntimeError(f"{server} did not become ready")
    return p


def start_publisher(server, live, logf):
    url = SERVERS[server]["rtmp"]
    if live:
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-re", *LAVFI,
               "-filter_complex", FILTER, *ENCODE, "-f", "flv", url]
    else:
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-re", "-stream_loop", "-1",
               "-i", SRC, "-c", "copy", "-f", "flv", url]
    return spawn(cmd, stdout=logf, stderr=subprocess.STDOUT)


def wait_playlist(server, timeout=20):
    """Waits until the stream is live on LL-HLS (media playlist has parts)."""
    url = SERVERS[server]["hls"]
    end = time.time() + timeout
    while time.time() < end:
        try:
            body = urllib.request.urlopen(url, timeout=1).read().decode()
            if "#EXT-X-STREAM-INF" in body:
                line = [l for l in body.splitlines() if l and not l.startswith("#")][0]
                body = urllib.request.urlopen(urllib.request.urljoin(url, line), timeout=2).read().decode()
            if body.count("#EXT-X-PART:") >= 3 and "#EXTINF" in body:
                return True
        except Exception:
            pass
        time.sleep(0.25)
    return False


# ---------------------------------------------------------------- scenarios

def machine():
    def sc(k):
        return subprocess.run(["sysctl", "-n", k], capture_output=True, text=True).stdout.strip()

    def ver(cmd):
        try:
            return subprocess.run(cmd, capture_output=True, text=True).stdout.splitlines()[0].strip()
        except Exception:
            return None

    git = subprocess.run(["git", "-C", ROOT, "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
    if LINUX:
        def first(path, key):
            try:
                return next(l.split(":", 1)[1].strip() for l in open(path) if l.startswith(key))
            except (OSError, StopIteration):
                return None
        mem_kb = first("/proc/meminfo", "MemTotal") or "0 kB"
        return {
            "cpu": first("/proc/cpuinfo", "model name"),
            "ncpu": str(os.cpu_count()),
            "mem_gb": round(int(mem_kb.split()[0]) / 2**20, 1),
            "os": (first("/etc/os-release", "PRETTY_NAME") or "Linux").strip('"') + f" ({platform.release()})",
            "ffmpeg": ver(["ffmpeg", "-version"]),
            "rustc": ver(["rustc", "--version"]),
            "mediamtx": ver([MTX_BIN, "--version"]),
            "caudal_git": git,
            "ulimit_n": resource.getrlimit(resource.RLIMIT_NOFILE)[0],
            "portrange": open("/proc/sys/net/ipv4/ip_local_port_range").read().split() if os.path.exists("/proc/sys/net/ipv4/ip_local_port_range") else None,
        }
    return {
        "cpu": sc("machdep.cpu.brand_string"),
        "ncpu": sc("hw.ncpu"),
        "perf_cores": sc("hw.perflevel0.physicalcpu"),
        "eff_cores": sc("hw.perflevel1.physicalcpu"),
        "mem_gb": round(int(sc("hw.memsize")) / 2**30, 1),
        "os": f"macOS {platform.mac_ver()[0]} ({sc('kern.osversion')})",
        "ffmpeg": ver(["ffmpeg", "-version"]),
        "rustc": ver(["rustc", "--version"]),
        "mediamtx": ver([MTX_BIN, "--version"]),
        "caudal_git": git,
        "ulimit_n": resource.getrlimit(resource.RLIMIT_NOFILE)[0],
        "portrange": f"{sc('net.inet.ip.portrange.first')}-{sc('net.inet.ip.portrange.last')}",
    }


def src_rates():
    out = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=codec_type,bit_rate",
                          "-of", "json", SRC], capture_output=True, text=True).stdout
    rates = {s["codec_type"]: int(s["bit_rate"]) / 1e6 for s in json.loads(out)["streams"]}
    return rates


def scenario_idle_publish(server, logf):
    """Idle RSS, then RSS + CPU with one publisher (file, -c copy)."""
    srv = start_server(server, logf)
    time.sleep(5)
    with Window({"server": srv.pid}) as w:
        time.sleep(15)
    idle = w.result["server"]
    pub = start_publisher(server, False, logf)
    ok = wait_playlist(server)
    time.sleep(10)
    with Window({"server": srv.pid, "publisher": pub.pid}) as w:
        time.sleep(30)
    publish = w.result
    publish["live_ok"] = ok
    stop(pub)
    stop(srv)
    return {"idle": idle, "publish": publish}


def scenario_fanout(server, proto, n, rates, logf):
    srv = start_server(server, logf)
    pub = start_publisher(server, False, logf)
    ok = wait_playlist(server)
    time.sleep(5)
    ramp_ms = min(10000, max(0, n * 10))
    warmup = ramp_ms / 1000.0 + 10.0
    duration = 30.0
    expect = rates["video"] + rates["audio"] if proto == "rtsp" else rates["video"]
    cmd = [CLIENT_BIN, proto, SERVERS[server][proto], "--viewers", str(n), "--warmup", str(warmup),
           "--duration", str(duration), "--ramp-ms", str(ramp_ms), "--expect-mbps", f"{expect:.3f}"]
    out_path = os.path.join(RESULTS, "client.out")
    k0 = kernel_counters()
    with open(out_path, "w") as out:
        cl = spawn(cmd, stdout=out, stderr=logf)
        # Sample strictly inside the client's own window (1 s margin each
        # side): a process that has exited reads as 0 CPU time in ps.
        time.sleep(warmup + 1)
        with Window({"server": srv.pid, "client": cl.pid, "publisher": pub.pid}) as w:
            time.sleep(duration - 2)
        try:
            cl.wait(30)
        except subprocess.TimeoutExpired:
            stop(cl)
    try:
        client = json.loads(open(out_path).read().strip().splitlines()[-1])
    except Exception as e:
        client = {"error": f"no client output: {e}"}
    k1 = kernel_counters()
    kernel = {k: (k1[k] - k0[k] if k1[k] is not None and k0[k] is not None else None) for k in k0}
    publisher_alive = pub.poll() is None
    stop(pub)
    stop(srv)
    return {"live_ok": ok, "procs": w.result, "client": client, "kernel": kernel,
            "publisher_alive_at_end": publisher_alive}


def scenario_latency_baseline(samples, logf):
    """Encoder -> FLV over TCP -> decoder, no server: the floor."""
    url = "tcp://127.0.0.1:9935"
    pub = spawn(["ffmpeg", "-hide_banner", "-loglevel", "error", "-re", *LAVFI, "-filter_complex", FILTER,
                 *ENCODE, "-f", "flv", url + "?listen=1"], stdout=logf, stderr=subprocess.STDOUT)
    time.sleep(1)
    try:
        out = subprocess.run([CLIENT_BIN, "latency-flv", url, "--samples", str(samples)], capture_output=True,
                             text=True, timeout=samples * 3 + 60).stdout
        res = json.loads(out.strip().splitlines()[-1])
    except Exception as e:
        res = {"error": str(e)}
    stop(pub)
    return res


def scenario_latency(server, samples, logf):
    srv = start_server(server, logf)
    pub = start_publisher(server, True, logf)
    ok = wait_playlist(server, 30)
    time.sleep(5)
    res = {"live_ok": ok}
    for mode, url in (("latency-hls", SERVERS[server]["hls"]), ("latency-rtsp", SERVERS[server]["rtsp"])):
        try:
            out = subprocess.run([CLIENT_BIN, mode, url, "--samples", str(samples)], capture_output=True,
                                 text=True, timeout=samples * 3 + 60).stdout
            res[mode] = json.loads(out.strip().splitlines()[-1])
        except Exception as e:
            res[mode] = {"error": str(e)}
    stop(pub)
    stop(srv)
    return res


# ---------------------------------------------------------------- main

def prepare():
    os.makedirs(CACHE, exist_ok=True)
    if os.path.exists(SRC):
        return
    log("rendering", SRC)
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", *LAVFI, "-t", "120",
                    "-filter_complex", FILTER, *ENCODE, SRC], check=True)


def run(argv):
    reps, only = 3, ["idle", "fanout", "latency"]
    levels, protos, samples = [1, 100, 300, 1000], ["hls", "rtsp", "whep"], 20
    servers = ["caudal", "mediamtx"]
    it = iter(argv)
    for a in it:
        if a == "--reps":
            reps = int(next(it))
        elif a == "--only":
            only = next(it).split(",")
        elif a == "--levels":
            levels = [int(x) for x in next(it).split(",")]
        elif a == "--protos":
            protos = next(it).split(",")
        elif a == "--servers":
            servers = next(it).split(",")
        elif a == "--samples":
            samples = int(next(it))
    try:
        resource.setrlimit(resource.RLIMIT_NOFILE, (65536, resource.getrlimit(resource.RLIMIT_NOFILE)[1]))
    except (ValueError, OSError):
        pass
    for s in servers:
        for binary in (SERVERS[s]["cmd"]()[0], CLIENT_BIN):
            if not os.path.exists(binary):
                sys.exit(f"missing {binary}; run bench/run.sh")
    prepare()
    os.makedirs(RESULTS, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    path = os.path.join(RESULTS, f"{stamp}.jsonl")
    logf = open(os.path.join(RESULTS, f"{stamp}.log"), "w")
    rates = src_rates()

    def emit(rec):
        rec["t"] = time.strftime("%Y-%m-%dT%H:%M:%S")
        with open(path, "a") as f:
            f.write(json.dumps(rec) + "\n")

    emit({"kind": "meta", "machine": machine(), "src_mbps": rates,
          "binary_mb": {"caudal": round(os.path.getsize(CAUDAL_BIN) / 1e6, 2),
                        "mediamtx": round(os.path.getsize(MTX_BIN) / 1e6, 2)}})
    for rep in range(reps):
        if "idle" in only:
            for s in servers:
                log(f"rep {rep} {s} idle+publish")
                emit({"kind": "idle_publish", "server": s, "rep": rep, **scenario_idle_publish(s, logf)})
        if "fanout" in only:
            for proto in protos:
                for n in levels:
                    for s in servers:
                        log(f"rep {rep} {s} fanout {proto} x{n}")
                        r = scenario_fanout(s, proto, n, rates, logf)
                        c = r["client"]
                        log(f"    egress {c.get('egress_mbps')} Mbps kept_up {c.get('kept_up')} "
                            f"err {c.get('errors')} to {c.get('timeouts')} lost {c.get('rtp_lost')} | "
                            f"server {r['procs']['server']} | client cpu {r['procs']['client']['cpu_pct']} | "
                            f"kernel {r['kernel']} pub_alive {r['publisher_alive_at_end']}")
                        emit({"kind": "fanout", "server": s, "proto": proto, "viewers": n, "rep": rep, **r})
                        time.sleep(3)
        if "latency" in only:
            log(f"rep {rep} baseline latency (no server)")
            r = scenario_latency_baseline(samples, logf)
            log("   ", r.get("median_ms"), r.get("p95_ms"))
            emit({"kind": "latency", "server": "none", "rep": rep, "latency-flv": r})
            for s in servers:
                log(f"rep {rep} {s} latency")
                r = scenario_latency(s, samples, logf)
                log("   ", {k: (v.get("median_ms"), v.get("p95_ms")) for k, v in r.items() if isinstance(v, dict)})
                emit({"kind": "latency", "server": s, "rep": rep, **r})
    log("results:", path)
    print(path)


# ---------------------------------------------------------------- report

def med_spread(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return "NOT MEASURED"
    m = statistics.median(xs)
    fmt = (lambda v: f"{v:.0f}") if m >= 100 else (lambda v: f"{v:.1f}") if m >= 1 else (lambda v: f"{v:.2f}")
    if len(xs) == 1:
        return fmt(m)
    return f"{fmt(m)} ({fmt(min(xs))}–{fmt(max(xs))})"


def report(path):
    recs = [json.loads(l) for l in open(path)]
    meta = next(r for r in recs if r["kind"] == "meta")
    out = []
    p = out.append
    p(f"Results file: `{os.path.relpath(path, ROOT)}`  ")
    p(f"Machine: {json.dumps(meta['machine'])}  ")
    p(f"Source: {json.dumps(meta['src_mbps'])} Mbps; binaries (MB): {json.dumps(meta['binary_mb'])}")
    p("")
    p("Cells: median (min–max) over repetitions.")
    p("")
    ip = [r for r in recs if r["kind"] == "idle_publish"]
    if ip:
        p("### Idle and one publisher")
        p("")
        p("| Metric | Caudal | MediaMTX |")
        p("|---|---|---|")
        rows = [
            ("Idle RSS, MB", lambda r: r["idle"]["rss_mb_median"]),
            ("Idle CPU, %", lambda r: r["idle"]["cpu_pct"]),
            ("1 publisher: RSS median, MB", lambda r: r["publish"]["server"]["rss_mb_median"]),
            ("1 publisher: RSS max, MB", lambda r: r["publish"]["server"]["rss_mb_max"]),
            ("1 publisher: CPU, % of one core", lambda r: r["publish"]["server"]["cpu_pct"]),
            ("(publisher ffmpeg CPU, %)", lambda r: r["publish"]["publisher"]["cpu_pct"]),
        ]
        for name, f in rows:
            cells = [med_spread([f(r) for r in ip if r["server"] == s]) for s in ("caudal", "mediamtx")]
            p(f"| {name} | {cells[0]} | {cells[1]} |")
        p("")
    fo = [r for r in recs if r["kind"] == "fanout"]
    if fo:
        p("### Fan-out")
        p("")
        p("CPU in % of one core (1,000 = 10 cores). Egress = payload bytes the client received "
          "(HTTP bodies / RTP payloads). Kept up = viewers that received >= 90% of real time "
          "(LL-HLS: media duration fetched vs wall time; RTSP/WHEP: >= 90% of the source bitrate).")
        p("")
        p("| Proto | Viewers | Server | Server CPU % | Server RSS MB (median) | RSS max | Egress Mbps | "
          "Kept up | Errors | Timeouts | RTP lost | Client CPU % | Client RSS MB | mbuf denied | UDP full drops | "
          "Publisher survived |")
        p("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
        for proto in ("hls", "rtsp", "whep"):
            if not any(r["proto"] == proto for r in fo):
                continue
            for n in sorted({r["viewers"] for r in fo}):
                for s in ("caudal", "mediamtx"):
                    rs = [r for r in fo if r["proto"] == proto and r["viewers"] == n and r["server"] == s]
                    if not rs:
                        continue
                    c = lambda k: [r["client"].get(k) for r in rs]
                    pr = lambda who, k: [r["procs"][who][k] for r in rs]
                    p(f"| {proto} | {n} | {s} | {med_spread(pr('server', 'cpu_pct'))} | "
                      f"{med_spread(pr('server', 'rss_mb_median'))} | {med_spread(pr('server', 'rss_mb_max'))} | "
                      f"{med_spread(c('egress_mbps'))} | {med_spread(c('kept_up'))} | {med_spread(c('errors'))} | "
                      f"{med_spread(c('timeouts'))} | {med_spread(c('rtp_lost'))} | "
                      f"{med_spread(pr('client', 'cpu_pct'))} | "
                      f"{med_spread(pr('client', 'rss_mb_median'))} | "
                      f"{med_spread([r.get('kernel', {}).get('mbuf_denied') for r in rs])} | "
                      f"{med_spread([r.get('kernel', {}).get('udp_full_drops') for r in rs])} | "
                      f"{sum(1 for r in rs if r.get('publisher_alive_at_end'))}/{len(rs)} |")
        p("")
    la = [r for r in recs if r["kind"] == "latency"]
    if la:
        p("### Latency (burned-in stamp -> decoded frame at the viewer)")
        p("")
        p("| Path | Server | Median ms | p95 ms | Samples | Per-run medians | PART-HOLD-BACK s |")
        p("|---|---|---|---|---|---|---|")
        for mode, servers in (("latency-flv", ("none",)), ("latency-hls", ("caudal", "mediamtx")),
                              ("latency-rtsp", ("caudal", "mediamtx"))):
            for s in servers:
                rs = [r[mode] for r in la if r["server"] == s and mode in r]
                if not rs:
                    continue
                allv = sorted(x for r in rs for x in r.get("samples_ms", []))
                if not allv:
                    p(f"| {mode[8:]} | {s} | NOT MEASURED | | | {rs[0].get('error') if rs else ''} | |")
                    continue
                q = lambda pc: allv[min(len(allv) - 1, round(pc / 100 * (len(allv) - 1)))]
                meds = ", ".join(str(r.get("median_ms")) for r in rs)
                hold = rs[0].get("part_hold_back_s", "") if mode == "latency-hls" else "—"
                p(f"| {mode[8:]} | {s} | {q(50):.0f} | {q(95):.0f} | {len(allv)} | {meds} | {hold} |")
        p("")
    print("\n".join(out))


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: (cleanup(), sys.exit(1)))
    try:
        if len(sys.argv) < 2:
            sys.exit(__doc__)
        cmd = sys.argv[1]
        if cmd == "prepare":
            prepare()
        elif cmd == "run":
            run(sys.argv[2:])
        elif cmd == "report":
            report(sys.argv[2])
        else:
            sys.exit(__doc__)
    finally:
        cleanup()
