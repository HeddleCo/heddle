#!/usr/bin/env bash
# Smoke-check program benchmark entry points without a full fixture or heddle binary.
#
# Exercises the *real* modules (not duplicated snippets):
#   - paired-bench.py (compile + n=3 true/true run)
#   - core_loop_absolute.py --self-test (summarize + printer)
#   - core-loop-bench.sh syntax
#
# Usage:
#   bash scripts/program/smoke-bench-scripts.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/heddle-bench-smoke-XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

echo "==> py_compile paired-bench.py + core_loop_absolute.py" >&2
python3 -m py_compile scripts/program/paired-bench.py scripts/program/core_loop_absolute.py

echo "==> bash -n core-loop-bench.sh" >&2
bash -n scripts/program/core-loop-bench.sh

echo "==> core_loop_absolute --self-test (real summarize + printer)" >&2
python3 scripts/program/core_loop_absolute.py --self-test

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

echo "==> smoke-bench-scripts: all checks passed" >&2
