#!/usr/bin/env python3
"""Summarizes a dhat-rs heap profile: the allocation sites holding the most
bytes at a chosen instant, named by their first Caudal frame.

    python3 bench/dhat_top.py dhat-heap.json [N] [--at-end]

By default the instant is the heap's peak (t-gmax): where the memory goes
under load. `--at-end` uses t-end instead — what was still allocated when
the process exited, which is where a leak shows: a site that grows with
churn and is never freed still holds its bytes after every publisher and
viewer is gone.
"""

import collections
import json
import re
import sys

# The global allocator shim sits in caudal's main.rs; it says nothing about
# who allocated.
SHIM = re.compile(r"__rust_\w*alloc|dhat")
# Frame files are relative to their crate: "(caudal-rtmp/src/lib.rs:82:33)".
OURS = re.compile(r"\(caudal[-a-z0-9]*/src/")
STD = re.compile(r"\((alloc|core|std)/src/|\(src/(raw_vec|vec|sync|alloc)/")


def site(frames, ftbl):
    """The innermost frame in Caudal's own crates, else the innermost frame
    outside the allocator, Rust's std and dhat."""
    names = [ftbl[i] for i in frames]
    for n in names:
        if OURS.search(n) and not SHIM.search(n):
            return n
    for n in names:
        if not SHIM.search(n) and not STD.search(n):
            return n
    return names[0] if names else "?"


def short(frame):
    # "0x1234: caudal_hls::packager::Packager::close_part (crates/caudal-hls/src/packager.rs:612:20)"
    frame = re.sub(r"^0x[0-9a-f]+: ", "", frame)
    frame = re.sub(r"::h[0-9a-f]{16}", "", frame)
    frame = re.sub(r"\(/.*?/(crates/|registry/src/[^/]+/)", "(", frame)
    return frame


def main():
    args = [a for a in sys.argv[1:] if a != "--at-end"]
    at_end = "--at-end" in sys.argv[1:]
    path = args[0]
    n = int(args[1]) if len(args) > 1 else 30
    d = json.load(open(path))
    ftbl, pps = d["ftbl"], d["pps"]
    # dhat's per-program-point keys: "gb"/"gbk" are bytes/blocks at t-gmax,
    # "fb"/"fbk" at t-end. Older profiles may not carry the t-end pair.
    bkey, kkey = ("fb", "fbk") if at_end else ("gb", "gbk")
    if at_end and not any(bkey in p for p in pps):
        sys.exit("this profile has no t-end figures; rerun without --at-end")
    for p in pps:
        p.setdefault(bkey, 0)
        p.setdefault(kkey, 0)
    total = sum(p[bkey] for p in pps)
    blocks = sum(p[kkey] for p in pps)
    when = f"at end (t-end {d['te'] / 1e6:.1f} s)" if at_end else f"at peak (t-gmax {d['tg'] / 1e6:.1f} s of {d['te'] / 1e6:.1f} s)"
    print(f"heap {when}: {total / 1e6:.1f} MB in {blocks} blocks")
    by_site = collections.Counter()
    by_crate = collections.Counter()
    for p in pps:
        s = short(site(p["fs"], ftbl))
        by_site[s] += p[bkey]
        m = re.match(r"<?(\w+)::", s)
        by_crate[m.group(1) if m else s[:40]] += p[bkey]
    label = "at end" if at_end else "at peak"
    print(f"\nby crate (MB {label}):")
    for c, b in by_crate.most_common(15):
        if b:
            print(f"  {b / 1e6:8.2f}  {c}")
    print(f"\ntop {n} sites (MB {label}):")
    for s, b in by_site.most_common(n):
        if b:
            print(f"  {b / 1e6:8.2f}  {s}")
    # The full stacks of the biggest ones, for when the first frame is not enough.
    print("\nstacks of the 8 biggest sites:")
    for p in sorted(pps, key=lambda p: -p[bkey])[:8]:
        print(f"-- {p[bkey] / 1e6:.2f} MB, {p[kkey]} blocks")
        for i in p["fs"][:14]:
            print("     " + short(ftbl[i]))


if __name__ == "__main__":
    main()
