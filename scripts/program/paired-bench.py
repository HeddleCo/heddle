#!/usr/bin/env python3
"""Paired alternating benchmark runner for equal-work comparisons.

Usage:
  python3 scripts/program/paired-bench.py \\
    --name status-json \\
    --trials 3 \\
    --a '/path/to/heddle status --output json' \\
    --b '/path/to/heddle status --output json' \\
    --cwd /path/to/fixture

Writes JSON stats to artifacts/perf/<stamp>-<name>.json by default.

Rules enforced by this tool's design:
  - Alternating A/B order (A B A B ...) so thermal/cache bias is shared.
  - Reports mean, median, p95, p99, min, max, stdev, and per-trial raw times.
  - Requires successful exit codes by default (--no-require-success to disable).
  - Does NOT claim wins; only prints ratios. Caller must ensure equal work
    and equivalent correctness outside this runner.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path


def percentile(sorted_vals: list[float], p: float) -> float:
    """Linear-interpolation percentile on a pre-sorted list."""
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


def run_once(
    cmd: str, cwd: Path | None, env: dict[str, str] | None
) -> tuple[float, int, str]:
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
    return elapsed, proc.returncode, proc.stderr or ""


def summarize(times: list[float]) -> dict:
    if not times:
        return {
            "n": 0,
            "mean_s": float("nan"),
            "median_s": float("nan"),
            "stdev_s": float("nan"),
            "min_s": float("nan"),
            "max_s": float("nan"),
            "p95_s": float("nan"),
            "p99_s": float("nan"),
            "raw_s": [],
        }
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


def ms(seconds: float) -> float:
    if math.isnan(seconds):
        return float("nan")
    return round(seconds * 1000.0, 3)


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--name", required=True, help="benchmark label used in output path")
    ap.add_argument("--trials", type=int, default=3, help="paired trial count (default 3)")
    ap.add_argument("--a", required=True, help="shell command A")
    ap.add_argument("--b", required=True, help="shell command B")
    ap.add_argument("--cwd", type=Path, default=None, help="working directory for both commands")
    ap.add_argument(
        "--warmup",
        type=int,
        default=1,
        help="alternating warmup rounds before timed trials (default 1)",
    )
    ap.add_argument(
        "--out",
        type=Path,
        default=None,
        help="output JSON path (default artifacts/perf/<stamp>-<name>.json)",
    )
    ap.add_argument(
        "--require-success",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="fail if any trial exits non-zero (default: true; use --no-require-success to disable)",
    )
    ap.add_argument(
        "--keep-stdout",
        action="store_true",
        default=False,
        help="capture stdout into the JSON (default: discard; timings only)",
    )
    args = ap.parse_args()
    if args.trials < 1:
        print("trials must be >= 1", file=sys.stderr)
        return 2
    if args.warmup < 0:
        print("warmup must be >= 0", file=sys.stderr)
        return 2

    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out = args.out or Path("artifacts/perf") / f"{stamp}-{args.name}.json"
    out.parent.mkdir(parents=True, exist_ok=True)

    # Inherit ambient env; never inject HEDDLE_PROFILE (would distort timings).
    base_env = os.environ.copy()
    base_env.pop("HEDDLE_PROFILE", None)

    def run_cmd(cmd: str) -> tuple[float, int, str]:
        if args.keep_stdout:
            start = time.perf_counter()
            proc = subprocess.run(
                cmd,
                shell=True,
                cwd=str(args.cwd) if args.cwd else None,
                env=base_env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            return time.perf_counter() - start, proc.returncode, proc.stderr or ""
        return run_once(cmd, args.cwd, base_env)

    # Warmup alternating once each
    for w in range(args.warmup):
        for label, cmd in (("A", args.a), ("B", args.b)):
            elapsed, rc, err = run_cmd(cmd)
            if args.require_success and rc != 0:
                print(
                    f"warmup {w} {label} failed rc={rc} elapsed={elapsed:.4f}s\n{err}",
                    file=sys.stderr,
                )
                return 1

    times_a: list[float] = []
    times_b: list[float] = []
    trials = []
    for i in range(args.trials):
        for label, cmd, bucket in (("A", args.a, times_a), ("B", args.b, times_b)):
            elapsed, rc, err = run_cmd(cmd)
            if args.require_success and rc != 0:
                print(
                    f"trial {i} {label} failed rc={rc} elapsed={elapsed:.4f}s\n{err}",
                    file=sys.stderr,
                )
                return 1
            bucket.append(elapsed)
            trials.append(
                {
                    "trial": i,
                    "label": label,
                    "seconds": elapsed,
                    "ms": ms(elapsed),
                    "exit_code": rc,
                }
            )

    summary_a = summarize(times_a)
    summary_b = summarize(times_b)
    med_a = summary_a.get("median_s")
    med_b = summary_b.get("median_s")
    if med_a and med_a > 0 and not math.isnan(med_a):
        ratio_median = med_b / med_a
    else:
        ratio_median = float("nan")

    # Convenience ms view alongside seconds for operators.
    def with_ms(summary: dict) -> dict:
        out_s = dict(summary)
        for key in ("mean_s", "median_s", "stdev_s", "min_s", "max_s", "p95_s", "p99_s"):
            if key in out_s:
                out_s[key.replace("_s", "_ms")] = ms(out_s[key])
        out_s["raw_ms"] = [ms(t) for t in out_s.get("raw_s", [])]
        return out_s

    payload = {
        "schema_version": 1,
        "kind": "paired_bench",
        "name": args.name,
        "timestamp_utc": stamp,
        "trials": args.trials,
        "warmup": args.warmup,
        "require_success": args.require_success,
        "cwd": str(args.cwd) if args.cwd else None,
        "command_a": args.a,
        "command_b": args.b,
        "a": with_ms(summary_a),
        "b": with_ms(summary_b),
        "ratio_b_over_a_median": ratio_median,
        "raw_trials": trials,
        "notes": (
            "Paired alternating A/B. Does not assert correctness or equal work; "
            "only times successful (or all) process wall times. "
            "Not a Git win claim; measurement calibration only."
        ),
    }
    out.write_text(json.dumps(payload, indent=2) + "\n")
    print(json.dumps(payload, indent=2))
    print(f"wrote {out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
