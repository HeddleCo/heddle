#!/usr/bin/env python3
"""Paired alternating benchmark runner for equal-work comparisons.

Usage:
  python3 scripts/program/paired-bench.py \\
    --name status-json \\
    --trials 3 \\
    --a 'cargo run -q -p heddle-cli -- status --output json' \\
    --b 'cargo run -q -p heddle-cli -- status --output json' \\
    --cwd /path/to/fixture

Writes JSON stats to artifacts/perf/<stamp>-<name>.json by default.

Rules enforced by this tool's design:
  - Alternating A/B order (A B A B ...) so thermal/cache bias is shared.
  - Reports mean, median, p95, p99, min, max, stdev, and per-trial raw times.
  - Does NOT claim wins; only prints ratios. Caller must ensure equal work
    and equivalent correctness outside this runner.
"""

from __future__ import annotations

import argparse
import json
import math
import shlex
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path


def percentile(sorted_vals: list[float], p: float) -> float:
    if not sorted_vals:
        return float("nan")
    if len(sorted_vals) == 1:
        return sorted_vals[0]
    k = (len(sorted_vals) - 1) * (p / 100.0)
    f = math.floor(k)
    c = math.ceil(k)
    if f == c:
        return sorted_vals[int(k)]
    return sorted_vals[f] * (c - k) + sorted_vals[c] * (k - f)


def run_once(cmd: str, cwd: Path | None, env: dict | None) -> tuple[float, int]:
    start = time.perf_counter()
    proc = subprocess.run(
        cmd,
        shell=True,
        cwd=str(cwd) if cwd else None,
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    elapsed = time.perf_counter() - start
    return elapsed, proc.returncode


def summarize(times: list[float]) -> dict:
    if not times:
        return {}
    s = sorted(times)
    return {
        "n": len(times),
        "mean_s": statistics.fmean(times),
        "median_s": statistics.median(times),
        "stdev_s": statistics.stdev(times) if len(times) > 1 else 0.0,
        "min_s": s[0],
        "max_s": s[-1],
        "p95_s": percentile(s, 95),
        "p99_s": percentile(s, 99),
        "raw_s": times,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--name", required=True)
    ap.add_argument("--trials", type=int, default=3)
    ap.add_argument("--a", required=True, help="shell command A")
    ap.add_argument("--b", required=True, help="shell command B")
    ap.add_argument("--cwd", type=Path, default=None)
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument(
        "--out",
        type=Path,
        default=None,
        help="output JSON path (default artifacts/perf/<stamp>-<name>.json)",
    )
    ap.add_argument(
        "--require-success",
        action="store_true",
        default=True,
        help="fail if any trial exits non-zero (default true)",
    )
    args = ap.parse_args()
    if args.trials < 1:
        print("trials must be >= 1", file=sys.stderr)
        return 2

    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out = args.out or Path("artifacts/perf") / f"{stamp}-{args.name}.json"
    out.parent.mkdir(parents=True, exist_ok=True)

    # Warmup alternating once each
    for _ in range(args.warmup):
        for label, cmd in (("A", args.a), ("B", args.b)):
            elapsed, rc = run_once(cmd, args.cwd, None)
            if args.require_success and rc != 0:
                print(f"warmup {label} failed rc={rc}", file=sys.stderr)
                return 1

    times_a: list[float] = []
    times_b: list[float] = []
    trials = []
    for i in range(args.trials):
        for label, cmd, bucket in (("A", args.a, times_a), ("B", args.b, times_b)):
            elapsed, rc = run_once(cmd, args.cwd, None)
            if args.require_success and rc != 0:
                print(f"trial {i} {label} failed rc={rc}", file=sys.stderr)
                return 1
            bucket.append(elapsed)
            trials.append({"trial": i, "label": label, "seconds": elapsed, "exit_code": rc})

    summary_a = summarize(times_a)
    summary_b = summarize(times_b)
    ratio_median = (
        summary_b["median_s"] / summary_a["median_s"]
        if summary_a.get("median_s")
        else float("nan")
    )
    payload = {
        "name": args.name,
        "timestamp_utc": stamp,
        "trials": args.trials,
        "cwd": str(args.cwd) if args.cwd else None,
        "command_a": args.a,
        "command_b": args.b,
        "a": summary_a,
        "b": summary_b,
        "ratio_b_over_a_median": ratio_median,
        "raw_trials": trials,
        "notes": (
            "Paired alternating A/B. Does not assert correctness or equal work; "
            "only times successful (or all) process wall times."
        ),
    }
    out.write_text(json.dumps(payload, indent=2) + "\n")
    print(json.dumps(payload, indent=2))
    print(f"wrote {out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
