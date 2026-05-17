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

## Failure modes the gate enforces

The gate is the only thing standing between a silent perf regression
and main. A measurement that *can't be checked* is treated as a
regression — silently passing on missing/corrupt input is the failure
mode Codex r1 flagged on PR #91 (P1). Hard failures:

  * Expected `(group, variant)` has no `estimates.json` (e.g. the
    bench was renamed, removed, or its build broke).
  * `estimates.json` exists but is unreadable (truncated, schema
    drift, JSON syntax error).
  * Criterion emits a `(group, variant)` that has no baseline entry
    (e.g. a new bench shipped without updating the baseline, or an
    existing bench was renamed and the old name is now stale).

The last case prevents a PR from adding a bench without committing
its baseline — the regression budget would silently exclude it.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


class EstimateError(Exception):
    """Raised when an expected estimate can't be loaded.

    Distinguished from a numeric over-budget regression: this is a
    *coverage* failure (the gate has no measurement to check against
    the baseline), not a *budget* failure. Both still result in exit
    code 1, but the script's output groups them separately so the
    reviewer can tell at a glance whether the bench actually got
    slower or whether the gate broke.
    """


def load_estimate(criterion_dir: Path, group: str, variant: str) -> float:
    """Return mean ns-per-iter for `<group>/<variant>`.

    Raises `EstimateError` if the file is missing or unreadable —
    a missing measurement is a coverage failure that the gate MUST
    treat as a regression (see module docstring).
    """
    estimates_path = criterion_dir / group / variant / "new" / "estimates.json"
    if not estimates_path.exists():
        raise EstimateError(f"estimate file not found: {estimates_path}")
    try:
        data = json.loads(estimates_path.read_text())
        return float(data["mean"]["point_estimate"])
    except (json.JSONDecodeError, KeyError, TypeError, ValueError) as exc:
        raise EstimateError(f"could not parse {estimates_path}: {exc}") from exc


def discover_actual_ids(criterion_dir: Path) -> set[tuple[str, str]]:
    """Walk criterion's output tree and return every `(group, variant)`
    pair that has an `estimates.json`.

    No scoping: every measurement found in `criterion_dir` is in-
    scope for the unexpected-ID check. A wholly-new top-level group
    (`bench_streaming_read` added to the bench without a baseline
    entry) and a variant rename inside an existing group both fail
    the gate. The earlier baseline-scoped version of this check was
    a silent-skip vector (Codex r2 P1) — adding any new group
    bypassed coverage entirely.

    Pre-condition: `criterion_dir` must contain *only* the fuse_e2e
    suite's output. CI guarantees this by running
    `rm -rf target/criterion` before `cargo bench` (Codex r2 P2 —
    `Swatinem/rust-cache` restores `target/` between runs, so a
    benchmark renamed in a previous run would leave residue that
    today's run wouldn't overwrite, producing false-positive
    "unexpected ID" failures). Locally, use a clean target dir or
    `rm -rf` the path before running the compare.

    Criterion's layout is `<group>/<variant>/new/estimates.json`, with
    one extra wrinkle: `<variant>` may itself be a nested path for
    `BenchmarkId::new("name", value)` ids (`name/value` segments). We
    walk for `new/estimates.json` files and reconstruct group/variant
    from the relative path.
    """
    found: set[tuple[str, str]] = set()
    if not criterion_dir.exists():
        return found
    for estimates_path in criterion_dir.rglob("new/estimates.json"):
        # `<criterion-dir>/<group>/<variant...>/new/estimates.json`.
        # `parts[-2]` is "new"; everything before that minus the
        # criterion_dir prefix gives us (group, *variant_parts).
        rel = estimates_path.relative_to(criterion_dir).parts
        # rel looks like (group, *variant, "new", "estimates.json").
        if len(rel) < 4 or rel[-2] != "new" or rel[-1] != "estimates.json":
            continue
        group = rel[0]
        # Criterion also writes per-group aggregate dirs (no variant);
        # those have rel == (group, "new", "estimates.json") which fails
        # the len < 4 check above. Anything we keep has variant parts.
        variant = "/".join(rel[1:-2])
        if not variant:
            continue
        found.add((group, variant))
    return found


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

    # Build the expected (group, variant) set from the baseline so we
    # can both (a) iterate it for budget checks, and (b) diff it
    # against what's actually on disk to detect untracked IDs.
    expected: set[tuple[str, str]] = set()
    for group, variants in baseline.items():
        if group.startswith("_"):
            continue
        for variant in variants:
            expected.add((group, variant))

    failures: list[str] = []
    missing: list[str] = []
    ok: list[str] = []

    for group, variant in sorted(expected):
        baseline_ns = float(baseline[group][variant]["ns_per_iter"])
        try:
            actual_ns = load_estimate(args.criterion_dir, group, variant)
        except EstimateError as exc:
            missing.append(f"{group}/{variant} ({exc})")
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

    # Coverage check: any `(group, variant)` in criterion that the
    # baseline doesn't know about. A rename produces both a missing
    # entry (above) and an unexpected entry (here); a variant
    # addition or a wholly-new top-level group produces only the
    # unexpected entry. Both are gate failures because the new
    # measurement isn't budgeted. See `discover_actual_ids` docs for
    # the criterion-dir cleanliness pre-condition.
    actual = discover_actual_ids(args.criterion_dir)
    unexpected = sorted(actual - expected)

    print("# fuse_e2e baseline comparison")
    print(f"# threshold: +{args.threshold * 100:.0f}% regression\n")
    if ok:
        print("## within budget")
        for line in ok:
            print(line)
        print()
    if missing:
        print("## MISSING (coverage failure — bench did not report)")
        for line in missing:
            print(line)
        print()
    if unexpected:
        print("## UNEXPECTED (coverage failure — bench ID has no baseline)")
        for group, variant in unexpected:
            print(f"{group:>20s}/{variant:<25s}  (commit a baseline entry or revert the rename)")
        print()
    if failures:
        print("## REGRESSIONS")
        for line in failures:
            print(line)
        print()

    coverage_errors = len(missing) + len(unexpected)
    if failures or coverage_errors:
        parts = []
        if failures:
            parts.append(f"{len(failures)} regression(s) over +{args.threshold * 100:.0f}%")
        if missing:
            parts.append(f"{len(missing)} missing measurement(s)")
        if unexpected:
            parts.append(f"{len(unexpected)} unexpected ID(s)")
        print(f"fuse_e2e: FAIL — {'; '.join(parts)}")
        return 1

    print(f"fuse_e2e: {len(ok)} measurement(s) within budget")
    return 0


if __name__ == "__main__":
    sys.exit(main())
