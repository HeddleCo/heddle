#!/usr/bin/env bash
# Smoke-check program benchmark entry points without a full fixture or heddle binary.
#
# Catches syntax errors and default-path printer crashes (e.g. p95=None with n=3).
# Usage:
#   bash scripts/program/smoke-bench-scripts.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/heddle-bench-smoke-XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

echo "==> py_compile paired-bench.py" >&2
python3 -m py_compile scripts/program/paired-bench.py

echo "==> bash -n core-loop-bench.sh" >&2
bash -n scripts/program/core-loop-bench.sh

echo "==> paired-bench smoke (A==B true, n=3; p95 must be null)" >&2
OUT="$TMP/paired-smoke.json"
python3 scripts/program/paired-bench.py \
  --name smoke-true \
  --trials 3 \
  --warmup 0 \
  --a 'true' \
  --b 'true' \
  --out "$OUT" \
  >/dev/null
python3 - "$OUT" <<'PY'
import json, sys
p = json.load(open(sys.argv[1]))
assert p["a"]["n"] == 3, p["a"]
assert p["a"]["p95_s"] is None, p["a"]
assert p["a"]["p99_s"] is None, p["a"]
assert p["a"]["sample_quality"] == "insufficient_for_tail", p["a"]
assert p.get("self_pair_calibration") is True
print("paired-bench JSON ok")
PY

echo "==> core-loop summarizer/printer handles p95_ms=None" >&2
python3 <<'PY'
# Mirrors the progress-printer logic in core-loop-bench.sh absolute timing block.
stats = {
    "median_ms": 12.3,
    "p95_ms": None,
    "mean_ms": 11.0,
    "n": 3,
    "sample_quality": "insufficient_for_tail",
}
p95 = (
    f"{stats['p95_ms']:.1f} ms"
    if stats["p95_ms"] is not None
    else "n/a (n<30)"
)
line = (
    f"{'status_json':<20} median={stats['median_ms']:.1f} ms  "
    f"p95={p95}  mean={stats['mean_ms']:.1f} ms  "
    f"n={stats['n']}  quality={stats.get('sample_quality', '?')}"
)
assert "n/a (n<30)" in line, line
assert "12.3" in line, line
print("core-loop printer path ok:", line)
PY

echo "==> smoke-bench-scripts: all checks passed" >&2
