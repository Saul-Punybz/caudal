#!/usr/bin/env python3
"""OMT CPU scenarios (M12), Caudal alone, on GitHub's Linux runners.

    python3 bench/omt.py [--only ingest,output] [--seconds 60] [--size 1920x1080] [--fps 60]

Never on a laptop (bench.yml runs it). Needs target/release/caudal, ffmpeg
(the same ffmpeg 8 the CI tests use) and the `omt` tool from the
open-media-transport repo at the rev Caudal pins (bench.yml installs it;
override with $OMT_CLI).

  ingest  `omt send` (colour bars + tone, VMX) -> [[omt.pull]] by omt://
          URL -> VMX decode -> ffmpeg (H.264/AAC) -> stream `omtin`.
          Measures Caudal + its ffmpeg children, and the sender.
  output  ffmpeg testsrc2 (H.264 + AAC over RTMP) -> stream `src` ->
          [[omt.output]] (H.264 decode -> VMX encode) -> `omt recv`.
          Measures Caudal + its children (the publisher is not one).

CPU is utime+stime from /proc over the measured window (after a warm-up),
as cores (1.0 = one core busy). Frame rates come from Caudal's own API
counters (ingest) and from `omt recv`'s last report (output). Prints a
Markdown table on stdout; logs and the raw JSON land in bench/results/.
"""

import json
import os
import re
import signal
import subprocess
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RESULTS = os.path.join(HERE, "results")
CAUDAL = os.path.join(ROOT, "target", "release", "caudal")
BASE = "http://127.0.0.1:8080"
TICK = os.sysconf("SC_CLK_TCK")
WARMUP_S = 10
CHILDREN = []


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
        p.wait(grace)
    except ProcessLookupError:
        return
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGKILL)
        p.wait()


def cleanup(*_):
    for p in CHILDREN:
        stop(p, 2.0)


def ticks(pid):
    """utime+stime of pid, in clock ticks, or None if gone."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            rest = f.read().rsplit(")", 1)[1].split()
        return int(rest[11]) + int(rest[12])
    except (OSError, IndexError, ValueError):
        return None


def descendants(pid):
    """pid and every process below it (ffmpeg runs in its own process
    group, but is still Caudal's child)."""
    parent = {}
    for d in os.listdir("/proc"):
        if d.isdigit():
            try:
                with open(f"/proc/{d}/stat") as f:
                    parent[int(d)] = int(f.read().rsplit(")", 1)[1].split()[1])
            except (OSError, IndexError, ValueError):
                pass
    out, todo = [pid], [pid]
    while todo:
        p = todo.pop()
        kids = [c for c, pp in parent.items() if pp == p]
        out += kids
        todo += kids
    return out


def cpu_window(pids_fn, seconds):
    """Cores used by the processes pids_fn() returns, over `seconds`.
    Re-lists them at the end so a child started mid-window counts from 0."""
    start = {p: ticks(p) for p in pids_fn()}
    t0 = time.monotonic()
    time.sleep(seconds)
    used = 0
    for p in pids_fn():
        now = ticks(p)
        if now is not None:
            used += now - (start.get(p) or 0)
    return used / TICK / (time.monotonic() - t0)


def get_json(path, timeout=2):
    with urllib.request.urlopen(BASE + path, timeout=timeout) as r:
        return json.load(r)


def wait_for(fn, what, timeout=60):
    end = time.time() + timeout
    last = None
    while time.time() < end:
        try:
            v = fn()
            if v:
                return v
        except Exception as e:  # noqa: BLE001 (any failure = not yet)
            last = e
        time.sleep(0.5)
    raise RuntimeError(f"timed out waiting for {what} ({last})")


def start_caudal(toml, logf):
    path = os.path.join(RESULTS, "omt-caudal.toml")
    with open(path, "w") as f:
        f.write(toml)
    p = spawn([CAUDAL, "--config", path], stdout=logf, stderr=subprocess.STDOUT)
    wait_for(lambda: urllib.request.urlopen(BASE + "/readyz", timeout=1).status == 200, "caudal ready", 30)
    return p


CAUDAL_BASE_TOML = """[server]
http_bind = "127.0.0.1:8080"
[rtmp]
bind = "127.0.0.1:1935"
[srt]
bind = "127.0.0.1:9000"
[webrtc]
udp_bind = "127.0.0.1:8189"
[moq]
enabled = false
"""


def scenario_ingest(omt_cli, size, fps, seconds, logf):
    sender = spawn([omt_cli, "send", "--name", "Caudal Bench", "--size", size, "--fps", str(fps)],
                   stdout=subprocess.PIPE, stderr=logf, text=True)
    line = sender.stdout.readline()
    logf.write(f"omt send: {line}")
    m = re.search(r"on port (\d+)", line)
    if not m:
        raise RuntimeError(f"omt send did not report a port: {line!r}")
    port = int(m.group(1))
    caudal = start_caudal(CAUDAL_BASE_TOML + f"""
[[omt.pull]]
stream = "omtin"
url = "omt://127.0.0.1:{port}"
video_kbps = 6000
""", logf)
    try:
        wait_for(lambda: get_json("/api/v1/streams/omtin")["tracks"], "stream omtin", 60)
        time.sleep(WARMUP_S)
        before = get_json("/api/v1/streams/omtin")["omt"]["pull"]
        t0 = time.monotonic()
        # Sender CPU over the same window, sampled around the Caudal window.
        s0 = ticks(sender.pid)
        caudal_cores = cpu_window(lambda: descendants(caudal.pid), seconds)
        s1 = ticks(sender.pid)
        elapsed = time.monotonic() - t0
        sender_cpu = ((s1 or 0) - (s0 or 0)) / TICK / elapsed
        after = get_json("/api/v1/streams/omtin")["omt"]["pull"]
        dropped = {k: after["dropped"][k] - before["dropped"].get(k, 0) for k in after["dropped"]}
        return {
            "scenario": "ingest",
            "size": size,
            "fps": fps,
            "caudal_cores": round(caudal_cores, 3),
            "sender_cores": round(sender_cpu, 3),
            "video_in_fps": round((after["video_in"] - before["video_in"]) / elapsed, 2),
            "reconnects": after["reconnects"] - before["reconnects"],
            "dropped": dropped,
        }
    finally:
        stop(caudal)
        stop(sender)


def scenario_output(omt_cli, size, fps, seconds, logf):
    caudal = start_caudal(CAUDAL_BASE_TOML + """
[[omt.output]]
stream = "src"
name = "Caudal Bench Out"
""", logf)
    w, h = size.split("x")
    pub = spawn(["ffmpeg", "-hide_banner", "-loglevel", "error", "-re",
                 "-f", "lavfi", "-i", f"testsrc2=size={w}x{h}:rate={fps}",
                 "-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000",
                 "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency",
                 "-g", str(fps * 2), "-b:v", "6000k", "-pix_fmt", "yuv420p",
                 "-c:a", "aac", "-b:a", "128k", "-f", "flv", "rtmp://127.0.0.1:1935/live/src"],
                stdout=logf, stderr=subprocess.STDOUT)
    recv = None
    try:
        def output_url():
            o = get_json("/api/v1/streams/src")["omt"]["outputs"][0]
            return o["url"]
        url = wait_for(output_url, "omt output url", 60)
        port = int(url.rsplit(":", 1)[1])
        recv = spawn([omt_cli, "recv", f"omt://127.0.0.1:{port}", "--seconds", str(WARMUP_S + seconds + 5)],
                     stdout=subprocess.PIPE, stderr=logf, text=True)
        time.sleep(WARMUP_S)
        before = get_json("/api/v1/streams/src")["omt"]["outputs"][0]
        t0 = time.monotonic()
        caudal_cores = cpu_window(lambda: descendants(caudal.pid), seconds)
        elapsed = time.monotonic() - t0
        after = get_json("/api/v1/streams/src")["omt"]["outputs"][0]
        stop(recv)
        lines = recv.stdout.read().splitlines()
        logf.write("\n".join(lines[-5:]) + "\n")
        reports = [float(m.group(1)) for m in (re.search(r"([\d.]+) fps received", l) for l in lines) if m]
        dropped = {k: after["dropped"][k] - before["dropped"].get(k, 0) for k in after["dropped"]}
        return {
            "scenario": "output",
            "size": size,
            "fps": fps,
            "caudal_cores": round(caudal_cores, 3),
            "frames_sent_fps": round((after["frames_sent"] - before["frames_sent"]) / elapsed, 2),
            "receiver_fps_last": reports[-1] if reports else None,
            "receivers": after["receivers"],
            "dropped": dropped,
        }
    finally:
        stop(recv)
        stop(pub)
        stop(caudal)


def main(argv):
    only, seconds, size, fps = ["ingest", "output"], 60, "1920x1080", 60
    i = 0
    while i < len(argv):
        a, v = argv[i], argv[i + 1] if i + 1 < len(argv) else None
        if a == "--only":
            only = [s for s in v.split(",") if s]
        elif a == "--seconds":
            seconds = int(v)
        elif a == "--size":
            size = v
        elif a == "--fps":
            fps = int(v)
        else:
            sys.exit(f"unknown option {a}")
        i += 2
    omt_cli = os.environ.get("OMT_CLI", "omt")
    os.makedirs(RESULTS, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    logf = open(os.path.join(RESULTS, f"omt-{stamp}.log"), "w", buffering=1)
    signal.signal(signal.SIGTERM, lambda *_: (cleanup(), sys.exit(1)))
    rows, failed = [], False
    try:
        for name in only:
            fn = {"ingest": scenario_ingest, "output": scenario_output}[name]
            log(f"omt {name}: {size} @ {fps} fps, {seconds} s after {WARMUP_S} s warm-up")
            try:
                rows.append(fn(omt_cli, size, fps, seconds, logf))
            except Exception as e:  # noqa: BLE001 (report and continue)
                failed = True
                log(f"omt {name} failed: {e}")
                rows.append({"scenario": name, "error": str(e)})
    finally:
        cleanup()
    with open(os.path.join(RESULTS, f"omt-{stamp}.json"), "w") as f:
        json.dump(rows, f, indent=1)
    print(f"### OMT CPU ({size} @ {fps} fps, {seconds} s window)\n")
    print("| scenario | Caudal cores (incl. ffmpeg) | other side cores | fps in Caudal | fps at receiver | drops |")
    print("|---|---:|---:|---:|---:|---|")
    for r in rows:
        if "error" in r:
            print(f"| {r['scenario']} | failed: {r['error']} | | | | |")
        elif r["scenario"] == "ingest":
            print(f"| ingest | {r['caudal_cores']} | {r['sender_cores']} (omt send) | {r['video_in_fps']} | - | {r['dropped']} |")
        else:
            print(f"| output | {r['caudal_cores']} | - | {r['frames_sent_fps']} | {r['receiver_fps_last']} | {r['dropped']} |")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
