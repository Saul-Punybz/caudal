#!/usr/bin/env python3
"""Summarizes a dhat-rs heap profile: the allocation sites holding the most
bytes at the heap's peak (t-gmax), named by their first Caudal frame.

    python3 bench/dhat_top.py dhat-heap.json [N]
"""

import collections
import json
import re
import sys

# Frames that say nothing about who allocated: the allocator, std
# containers, and third-party buffer types.
NOISE = re.compile(
    r"\b(dhat|alloc|core|std|hashbrown|bytes|__rust|rust_begin|<\?>|tokio::runtime|smallvec)\b|\[root\]"
)


def site(frames, ftbl):
    """First frame worth reading, preferring Caudal's own code."""
    names = [ftbl[i] for i in frames]
    for n in names:
        if "caudal" in n and not NOISE.search(n.split(":", 1)[-1].split("(")[0]):
            return n
    for n in names:
        if not NOISE.search(n.split(":", 1)[-1].split("(")[0]):
            return n
    return names[0] if names else "?"


def short(frame):
    # "0x1234: caudal_hls::packager::Packager::close_part (crates/caudal-hls/src/packager.rs:612:20)"
    frame = re.sub(r"^0x[0-9a-f]+: ", "", frame)
    frame = re.sub(r"::h[0-9a-f]{16}", "", frame)
    frame = re.sub(r"\(/.*?/(crates/|registry/src/[^/]+/)", "(", frame)
    return frame


def main():
    path = sys.argv[1]
    n = int(sys.argv[2]) if len(sys.argv) > 2 else 30
    d = json.load(open(path))
    ftbl, pps = d["ftbl"], d["pps"]
    total = sum(p["gb"] for p in pps)
    blocks = sum(p["gbk"] for p in pps)
    print(f"heap at peak (t-gmax {d['tg'] / 1e6:.1f} s of {d['te'] / 1e6:.1f} s): "
          f"{total / 1e6:.1f} MB in {blocks} blocks")
    by_site = collections.Counter()
    by_crate = collections.Counter()
    for p in pps:
        s = short(site(p["fs"], ftbl))
        by_site[s] += p["gb"]
        m = re.match(r"<?(\w+)::", s)
        by_crate[m.group(1) if m else s[:40]] += p["gb"]
    print("\nby crate (MB at peak):")
    for c, b in by_crate.most_common(15):
        if b:
            print(f"  {b / 1e6:8.2f}  {c}")
    print(f"\ntop {n} sites (MB at peak):")
    for s, b in by_site.most_common(n):
        if b:
            print(f"  {b / 1e6:8.2f}  {s}")
    # The full stacks of the biggest ones, for when the first frame is not enough.
    print("\nstacks of the 8 biggest sites:")
    for p in sorted(pps, key=lambda p: -p["gb"])[:8]:
        print(f"-- {p['gb'] / 1e6:.2f} MB, {p['gbk']} blocks")
        for i in p["fs"][:14]:
            print("     " + short(ftbl[i]))


if __name__ == "__main__":
    main()
