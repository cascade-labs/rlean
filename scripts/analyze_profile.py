#!/usr/bin/env python3
"""Aggregate a samply (Firefox Profiler) profile into a human-readable report.

Reads a gzipped ``profile.json.gz`` produced by ``samply record`` and prints:
  * total CPU-time and per-thread breakdown
  * self-time grouped by shared library (where CPU actually burns)
  * self-time grouped by subsystem (iceberg / tokio / python / clones / ...)
  * top self functions and top inclusive functions, symbolicated via ``atos``

Native (Rust) frames in the target binary are stored as raw file offsets in the
profile; we batch-symbolicate them with macOS ``atos`` against the built binary.

Usage:
    scripts/analyze_profile.py <profile.json.gz> [--binary target/debug/rlean]
                               [--top N] [--workdir DIR]
"""

from __future__ import annotations

import argparse
import collections
import gzip
import json
import shutil
import subprocess
import sys
from pathlib import Path


def get_strings(thread: dict) -> list | None:
    """The string table moved around across profile formats; handle each shape."""
    if "stringArray" in thread:
        return thread["stringArray"]
    st = thread.get("stringTable")
    if isinstance(st, list):
        return st
    if isinstance(st, dict):
        return st.get("_array")
    return None


def categorize(name: str) -> str:
    n = name.lower()
    if "iceberg" in n:
        return "iceberg (scan/manifest planning)"
    if "parquet" in n:
        return "parquet decode"
    if "arrow" in n:
        return "arrow"
    if "datafusion" in n:
        return "datafusion"
    if "tokio" in n or "futures" in n:
        return "tokio/futures runtime"
    if "pyo3" in n or "python" in n or "lean_python" in n:
        return "python bridge"
    if "rlean_storage" in n:
        return "rlean-storage"
    if "rlean_engine" in n:
        return "rlean-engine"
    if "rlean_data" in n:
        return "rlean-data"
    if "rlean_algorithm" in n:
        return "rlean-algorithm"
    if "rlean_core" in n or "symbol" in n:
        return "rlean-core"
    if "decimal" in n:
        return "rust_decimal"
    if "serde" in n or "json" in n:
        return "serde/json"
    if "hashbrown" in n or "hashmap" in n:
        return "hashmap"
    if "clone" in n:
        return "clones"
    if "drop_in_place" in n:
        return "drops/dealloc"
    if "malloc" in n or "free" in n:
        return "allocator"
    return "other"


def symbolicate(binary: Path, workdir: Path, offsets: list[int]) -> dict[int, str]:
    """Resolve a set of file offsets in ``binary`` to symbol names via ``atos``."""
    sym: dict[int, str] = {}
    if not offsets:
        return sym
    if shutil.which("atos") is None or not binary.exists():
        return {a: hex(a) for a in offsets}
    ordered = sorted(offsets)
    batch = 500
    for i in range(0, len(ordered), batch):
        chunk = ordered[i : i + batch]
        try:
            out = subprocess.run(
                ["atos", "-o", str(binary), "-arch", "arm64", "-offset",
                 *[hex(a) for a in chunk]],
                capture_output=True, text=True, cwd=str(workdir), check=False,
            )
        except OSError:
            return {a: hex(a) for a in ordered}
        for addr, line in zip(chunk, out.stdout.strip().split("\n")):
            sym[addr] = line.strip() or hex(addr)
    return sym


def short(name: str) -> str:
    return name.split(" (in ")[0]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("profile", type=Path, help="path to profile.json.gz from samply")
    ap.add_argument("--binary", type=Path, default=Path("target/debug/rlean"),
                    help="built binary used to symbolicate native frames")
    ap.add_argument("--binary-libname", default="rlean",
                    help="library name of the target binary inside the profile")
    ap.add_argument("--workdir", type=Path, default=Path.cwd(),
                    help="directory atos resolves binary-relative paths against")
    ap.add_argument("--top", type=int, default=30, help="rows per table")
    args = ap.parse_args()

    if not args.profile.exists():
        print(f"error: profile not found: {args.profile}", file=sys.stderr)
        return 1

    with gzip.open(args.profile) as fh:
        d = json.load(fh)

    interval = d["meta"].get("interval", 1.0)  # ms per sample
    libs = d["libs"]
    libname = [l.get("name", "?") for l in libs]
    target_lib = next((i for i, l in enumerate(libs)
                       if l.get("name") == args.binary_libname), None)

    # ---- collect every native offset in the target binary to symbolicate ----
    all_addrs: set[int] = set()
    for t in d["threads"]:
        ft = t["frameTable"]
        funcT = t["funcTable"]
        res = t["resourceTable"]
        res_lib = res.get("lib")
        func_res = funcT["resource"]
        frame_func = ft["func"]
        frame_addr = ft["address"]
        for fi in range(ft["length"]):
            r = func_res[frame_func[fi]]
            li = res_lib[r] if (r is not None and r >= 0) else None
            if li == target_lib:
                a = frame_addr[fi]
                if a is not None and a >= 0:
                    all_addrs.add(a)
    sym = symbolicate(args.binary, args.workdir, list(all_addrs))

    def frame_name(t, strings, fr) -> str:
        ft = t["frameTable"]
        funcT = t["funcTable"]
        res = t["resourceTable"]
        fn = ft["func"][fr]
        r = funcT["resource"][fn]
        li = res["lib"][r] if (r is not None and r >= 0) else None
        if li == target_lib:
            a = ft["address"][fr]
            return short(sym.get(a, strings[funcT["name"][fn]]))
        prefix = libname[li] if li is not None else "unknown"
        return f"{prefix}::{strings[funcT['name'][fn]]}"

    # ---- aggregate ----
    total = 0
    thread_samples = collections.Counter()
    lib_self = collections.Counter()
    self_fn = collections.Counter()
    incl_fn = collections.Counter()
    subsystem = collections.Counter()

    for t in d["threads"]:
        strings = get_strings(t)
        if strings is None:
            continue
        tname = t.get("name", "unknown")
        ft = t["frameTable"]
        funcT = t["funcTable"]
        stackT = t["stackTable"]
        res = t["resourceTable"]
        res_lib = res.get("lib")
        func_res = funcT["resource"]
        frame_func = ft["func"]
        stack_frame = stackT["frame"]
        stack_prefix = stackT["prefix"]

        for st_idx in t["samples"]["stack"]:
            if st_idx is None:
                continue
            total += 1
            thread_samples[tname] += 1

            leaf = stack_frame[st_idx]
            self_fn[frame_name(t, strings, leaf)] += 1
            r = func_res[frame_func[leaf]]
            li = res_lib[r] if (r is not None and r >= 0) else None
            lib_self[libname[li] if li is not None else "unknown"] += 1

            seen_fn: set[str] = set()
            seen_cat: set[str] = set()
            s = st_idx
            while s is not None:
                fr = stack_frame[s]
                nm = frame_name(t, strings, fr)
                if nm not in seen_fn:
                    seen_fn.add(nm)
                    incl_fn[nm] += 1
                cat = categorize(nm)
                if cat not in seen_cat:
                    seen_cat.add(cat)
                    subsystem[cat] += 1
                s = stack_prefix[s]

    if total == 0:
        print("error: profile contained no CPU samples", file=sys.stderr)
        return 1

    def secs(c: int) -> float:
        return c * interval / 1000.0

    def table(title, counter, n, width=110, skip=()):
        print(f"\n#### {title} ####")
        shown = 0
        for name, c in counter.most_common():
            if name in skip:
                continue
            print(f"{secs(c):8.1f}s  {100 * c / total:5.1f}%  {name[:width]}")
            shown += 1
            if shown >= n:
                break

    print(f"samples={total}  interval={interval}ms  "
          f"total_cpu_time={secs(total):.1f}s")
    table("CPU-TIME BY THREAD", thread_samples, args.top, width=60)
    table("SELF-TIME BY LIBRARY", lib_self, args.top, width=60)
    table("SELF-TIME BY SUBSYSTEM (dedup per sample)", subsystem, args.top,
          width=60, skip=("other",))
    table("TOP SELF FUNCTIONS", self_fn, args.top)
    table("TOP INCLUSIVE FUNCTIONS", incl_fn, args.top)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
