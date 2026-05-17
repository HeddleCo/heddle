#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Compare a fresh `fuse_e2e` criterion run to the committed baseline.

Reads each `<workload>/<variant>/estimates.json` under criterion's
`target/criterion/` output tree, looks up the matching entry in
`crates/mount/benches/fuse_e2e_baseline.json`, and fails if the new
mean exceeds `baseline * (1 + threshold)`.

Default threshold: 20% (matches HeddleCo/heddle#89 acceptance
criterion). Override via `--threshold 0.10` for tighter gates on
specific workloads.

Usage:
    python3 scripts/fuse-bench-compare.py \\
        --criterion-dir .heddleco-orchestrator/target/heddle/criterion \\
        --baseline crates/mount/benches/fuse_e2e_baseline.json \\
        --threshold 0.20

Exits 0 if every measurement is within budget; exits 1 with a
human-readable diff otherwise. Designed for the `fuse-bench` CI job;
output also works as a local smoke check.

Baseline file shape (committed, hand-edited when intentional):

    {
      "_meta": {
        "captured_on": "2026-05-17 / ubuntu-latest / 4 vCPU",
        "regenerate_with": "cargo bench --features fuse -p heddle-mount --bench fuse_e2e",
        "note": "Update when intentional perf changes land; ratchet down — never up — without justification."
      },
      "seq_read": {
        "heddle": {"ns_per_iter": 12345678.0, "throughput_mib_s": 162.0},
        "vanilla": {"ns_per_iter": 234567.0, "throughput_mib_s": 8500.0}
      },
      ...
    }

Criterion's `estimates.json` lives at
`<criterion-dir>/<group>/<id>/new/estimates.json` and exposes
`mean.point_estimate` in nanoseconds per iteration. We use the mean
(not the median) because that's what `cargo bench` reports as the
canonical figure and what existing heddle perf notes cite.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def load_estimate(criterion_dir: Path, group: str, variant: str) -> float | None:
    """Return mean ns-per-iter for `<group>/<variant>` or None if missing.

    Criterion writes estimates to `.../new/estimates.json` after each
    completed run. The structure carries the bootstrap distribution;
    we want the point estimate of the mean for direct comparison
    against the baseline.
    """
    estimates_path = criterion_dir / group / variant / "new" / "estimates.json"
    if not estimates_path.exists():
        return None
    try:
        data = json.loads(estimates_path.read_text())
        return float(data["mean"]["point_estimate"])
    except (json.JSONDecodeError, KeyError, TypeError) as exc:
        print(f"warn: could not parse {estimates_path}: {exc}", file=sys.stderr)
        return None


def main() -> int:
    ap = argparse.ArgumentParser(description="Compare fuse_e2e to baseline.")
    ap.add_argument(
        "--criterion-dir",
        type=Path,
        required=True,
        help="Path to criterion's output dir (target/.../criterion).",
    )
    ap.add_argument(
        "--baseline",
        type=Path,
        required=True,
        help="Path to committed baseline JSON.",
    )
    ap.add_argument(
        "--threshold",
        type=float,
        default=0.20,
        help="Fractional regression tolerance (default 0.20 = +20%%).",
    )
    args = ap.parse_args()

    if not args.criterion_dir.exists():
        print(
            f"error: criterion dir {args.criterion_dir} not found — "
            f"did the bench actually run?",
            file=sys.stderr,
        )
        return 1
    if not args.baseline.exists():
        print(f"error: baseline {args.baseline} not found", file=sys.stderr)
        return 1

    baseline = json.loads(args.baseline.read_text())

    failures: list[str] = []
    skipped: list[str] = []
    ok: list[str] = []

    for group, variants in baseline.items():
        if group.startswith("_"):
            continue
        for variant, expected in variants.items():
            baseline_ns = float(expected["ns_per_iter"])
            actual_ns = load_estimate(args.criterion_dir, group, variant)
            if actual_ns is None:
                skipped.append(f"{group}/{variant} (no estimate file)")
                continue
            ratio = actual_ns / baseline_ns
            delta_pct = (ratio - 1.0) * 100.0
            line = (
                f"{group:>20s}/{variant:<25s}  "
                f"baseline={baseline_ns / 1e6:>10.3f} ms  "
                f"actual={actual_ns / 1e6:>10.3f} ms  "
                f"delta={delta_pct:+6.1f}%"
            )
            if ratio > (1.0 + args.threshold):
                failures.append(line)
            else:
                ok.append(line)

    print("# fuse_e2e baseline comparison")
    print(f"# threshold: +{args.threshold * 100:.0f}% regression\n")
    if ok:
        print("## within budget")
        for line in ok:
            print(line)
        print()
    if skipped:
        print("## skipped")
        for line in skipped:
            print(line)
        print()
    if failures:
        print("## REGRESSIONS")
        for line in failures:
            print(line)
        print()
        print(f"fuse_e2e: {len(failures)} regression(s) over the +{args.threshold * 100:.0f}% threshold")
        return 1

    print(f"fuse_e2e: {len(ok)} measurement(s) within budget")
    return 0


if __name__ == "__main__":
    sys.exit(main())
