#!/usr/bin/env python3
"""Absolute multi-op core-loop timings for the equal-work fixture.

Extracted from core-loop-bench.sh so smoke/unit checks exercise the real
implementation (not a duplicated printer snippet).

Usage (invoked by core-loop-bench.sh):
  python3 scripts/program/core_loop_absolute.py \\
    --heddle /path/to/heddle \\
    --fixture /path/to/fixture \\
    --config /path/to/config.toml \\
    --trials 3 --warmup 1 \\
    --out artifacts/perf/stamp-core-loop-absolute.json \\
    --commit HEAD --branch main --stamp 20260101T000000Z \\
    --file-count 300 --thread-count 24 \\
    --ops-json '[{"id":"help","args":["help"]}]'

Self-test (no heddle binary):
  python3 scripts/program/core_loop_absolute.py --self-test
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
from pathlib import Path

# p95/p99 require this many samples; below that fields are null.
MIN_TAIL_N = 30


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


def summarize(times: list[float]) -> dict:
    """Summarize wall times; null p95/p99 when n < MIN_TAIL_N."""
    if not times:
        return {
            "n": 0,
            "mean_s": float("nan"),
            "median_s": float("nan"),
            "stdev_s": float("nan"),
            "min_s": float("nan"),
            "max_s": float("nan"),
            "p95_s": None,
            "p99_s": None,
            "sample_quality": "empty",
            "mean_ms": float("nan"),
            "median_ms": float("nan"),
            "stdev_ms": float("nan"),
            "p95_ms": None,
            "p99_ms": None,
            "min_ms": float("nan"),
            "max_ms": float("nan"),
            "raw_s": [],
            "raw_ms": [],
        }
    s = sorted(times)
    n = len(times)
    mean = statistics.fmean(times)
    med = statistics.median(times)
    stdev = statistics.stdev(times) if n > 1 else 0.0
    tail_ok = n >= MIN_TAIL_N
    p95 = percentile(s, 95) if tail_ok else None
    p99 = percentile(s, 99) if tail_ok else None
    return {
        "n": n,
        "mean_s": mean,
        "median_s": med,
        "stdev_s": stdev,
        "min_s": s[0],
        "max_s": s[-1],
        "p95_s": p95,
        "p99_s": p99,
        "sample_quality": "tail_ok" if tail_ok else "insufficient_for_tail",
        "mean_ms": mean * 1000.0,
        "median_ms": med * 1000.0,
        "stdev_ms": stdev * 1000.0,
        "p95_ms": None if p95 is None else p95 * 1000.0,
        "p99_ms": None if p99 is None else p99 * 1000.0,
        "min_ms": s[0] * 1000.0,
        "max_ms": s[-1] * 1000.0,
        "raw_s": times,
        "raw_ms": [t * 1000.0 for t in times],
    }


def format_op_progress_line(op_id: str, stats: dict) -> str:
    """Human progress line for one op (must tolerate p95_ms=None)."""
    p95 = (
        f"{stats['p95_ms']:.1f} ms"
        if stats.get("p95_ms") is not None
        else f"n/a (n<{MIN_TAIL_N})"
    )
    return (
        f"{op_id:<20} median={stats['median_ms']:.1f} ms  "
        f"p95={p95}  mean={stats['mean_ms']:.1f} ms  "
        f"n={stats['n']}  quality={stats.get('sample_quality', '?')}"
    )


def run_absolute(
    *,
    heddle: str,
    fixture: Path,
    config: str,
    trials: int,
    warmup: int,
    out_path: Path,
    commit: str,
    branch: str,
    stamp: str,
    file_count: int,
    thread_count: int,
    ops: list[dict],
) -> dict:
    env = os.environ.copy()
    env["HEDDLE_CONFIG"] = config
    env.pop("HEDDLE_PROFILE", None)

    results: list[dict] = []
    for op in ops:
        args = list(op["args"])
        cmd = [heddle, *args]
        for _ in range(warmup):
            proc = subprocess.run(
                cmd,
                cwd=str(fixture),
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
            )
            if proc.returncode != 0:
                raise SystemExit(
                    f"warmup failed for {op['id']}: rc={proc.returncode}\n{proc.stderr}"
                )
        times: list[float] = []
        raw_trials: list[dict] = []
        for i in range(trials):
            start = time.perf_counter()
            proc = subprocess.run(
                cmd,
                cwd=str(fixture),
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
            )
            elapsed = time.perf_counter() - start
            if proc.returncode != 0:
                raise SystemExit(
                    f"trial {i} failed for {op['id']}: rc={proc.returncode}\n{proc.stderr}"
                )
            times.append(elapsed)
            raw_trials.append(
                {
                    "trial": i,
                    "seconds": elapsed,
                    "ms": elapsed * 1000.0,
                    "exit_code": 0,
                }
            )
        stats = summarize(times)
        results.append(
            {
                "id": op["id"],
                "argv": cmd,
                "stats": stats,
                "raw_trials": raw_trials,
            }
        )
        print(f"  {format_op_progress_line(op['id'], stats)}", file=sys.stderr)

    payload = {
        "schema_version": 1,
        "kind": "core_loop_absolute",
        "timestamp_utc": stamp,
        "commit": commit,
        "branch": branch,
        "heddle_bin": heddle,
        "fixture": str(fixture),
        "fixture_recipe": {
            "files": file_count,
            "dir_modulus": 20,
            "threads": thread_count,
            "dirty_file": "tracked-00/file-000.txt",
            "seed_message": "seed",
            "matches": "crates/cli/tests/cli_integration/perf_core_loop.rs::setup_core_loop_fixture",
        },
        "trials": trials,
        "warmup": warmup,
        "min_tail_n": MIN_TAIL_N,
        "require_success": True,
        "operations": results,
        "disclaimer": (
            "Measurement calibration on a fixed equal-work fixture. "
            "Not a Git win claim. Absolute wall-clock process times only; "
            "no behavior skipped, no early-exit gaming. "
            f"p95/p99 are null when n < {MIN_TAIL_N}."
        ),
    }
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(payload, indent=2) + "\n")
    print(f"wrote {out_path}", file=sys.stderr)
    return payload


def self_test() -> None:
    """Executable smoke without a heddle binary — tests real module code."""
    # n=3: tails suppressed
    s3 = summarize([0.01, 0.02, 0.015])
    assert s3["n"] == 3
    assert s3["p95_ms"] is None
    assert s3["p99_ms"] is None
    assert s3["sample_quality"] == "insufficient_for_tail"
    line = format_op_progress_line("status_json", s3)
    assert f"n/a (n<{MIN_TAIL_N})" in line, line
    assert "status_json" in line
    # Formatting None must not raise (the regression Codex caught).
    _ = f"  {line}"

    # n>=MIN_TAIL_N: tails present
    big = [0.01 + (i * 0.0001) for i in range(MIN_TAIL_N)]
    s30 = summarize(big)
    assert s30["n"] == MIN_TAIL_N
    assert s30["p95_ms"] is not None
    assert s30["p99_ms"] is not None
    assert s30["sample_quality"] == "tail_ok"
    line30 = format_op_progress_line("help", s30)
    assert "n/a" not in line30, line30
    assert "ms" in line30

    empty = summarize([])
    assert empty["n"] == 0
    assert empty["sample_quality"] == "empty"

    print("core_loop_absolute self-test ok", file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--self-test", action="store_true", help="run unit smoke without heddle")
    ap.add_argument("--heddle", help="path to heddle binary")
    ap.add_argument("--fixture", type=Path, help="fixture working directory")
    ap.add_argument("--config", help="HEDDLE_CONFIG path")
    ap.add_argument("--trials", type=int, default=3)
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument("--out", type=Path, help="output JSON path")
    ap.add_argument("--commit", default="unknown")
    ap.add_argument("--branch", default="unknown")
    ap.add_argument("--stamp", default="")
    ap.add_argument("--file-count", type=int, default=300)
    ap.add_argument("--thread-count", type=int, default=24)
    ap.add_argument("--ops-json", help="JSON array of {id, args}")
    args = ap.parse_args(argv)

    if args.self_test:
        self_test()
        return 0

    missing = [
        name
        for name, val in (
            ("--heddle", args.heddle),
            ("--fixture", args.fixture),
            ("--config", args.config),
            ("--out", args.out),
            ("--ops-json", args.ops_json),
        )
        if not val
    ]
    if missing:
        ap.error(f"missing required args (or use --self-test): {', '.join(missing)}")

    if args.trials < 1:
        ap.error("--trials must be >= 1")
    if args.warmup < 0:
        ap.error("--warmup must be >= 0")

    ops = json.loads(args.ops_json)
    run_absolute(
        heddle=args.heddle,
        fixture=args.fixture,
        config=args.config,
        trials=args.trials,
        warmup=args.warmup,
        out_path=args.out,
        commit=args.commit,
        branch=args.branch,
        stamp=args.stamp or "unstamped",
        file_count=args.file_count,
        thread_count=args.thread_count,
        ops=ops,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
