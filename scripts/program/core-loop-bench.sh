#!/usr/bin/env bash
# Equal-work core-loop fixture + multi-command absolute timing runner.
#
# Creates a FIXED fixture matching crates/cli/tests/cli_integration/perf_core_loop.rs
# (300 even-spread files, seed capture, 24 threads, one dirty tracked file), then
# times release-binary core-loop commands with multiple trials.
#
# Usage:
#   bash scripts/program/core-loop-bench.sh
#   bash scripts/program/core-loop-bench.sh --heddle target/release/heddle --trials 3
#   bash scripts/program/core-loop-bench.sh --keep-fixture /tmp/heddle-core-loop-fixture
#
# Outputs:
#   artifacts/perf/<stamp>-core-loop-absolute.json   multi-op absolute timings
#   artifacts/perf/<stamp>-core-loop-<op>.json       optional per-op paired-bench (A==B)
#
# This is measurement calibration, NOT a Git comparison or win claim.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

HEDDLE_BIN="${HEDDLE_BIN:-}"
TRIALS=3
WARMUP=1
KEEP_FIXTURE=""
OUT_DIR="${OUT_DIR:-$ROOT/artifacts/perf}"
RUN_PAIRED=1
FILE_COUNT=300
THREAD_COUNT=24

usage() {
  cat <<'EOF'
Usage: core-loop-bench.sh [options]

  --heddle PATH       Path to heddle release binary (default: target/release/heddle)
  --trials N          Timed trials per operation (default: 3)
  --warmup N          Warmup rounds per operation (default: 1)
  --out-dir DIR       Artifact directory (default: artifacts/perf)
  --keep-fixture DIR  Keep fixture at DIR instead of temp cleanup
  --no-paired         Skip per-op paired-bench A==B JSON (absolute multi-op only)
  --file-count N      Tracked files in fixture (default: 300; match perf_core_loop.rs)
  --thread-count N    Threads to create (default: 24; match perf_core_loop.rs)
  -h, --help          Show this help

Environment:
  HEDDLE_BIN          Same as --heddle
  OUT_DIR             Same as --out-dir
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --heddle) HEDDLE_BIN="$2"; shift 2 ;;
    --trials) TRIALS="$2"; shift 2 ;;
    --warmup) WARMUP="$2"; shift 2 ;;
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    --keep-fixture) KEEP_FIXTURE="$2"; shift 2 ;;
    --no-paired) RUN_PAIRED=0; shift ;;
    --file-count) FILE_COUNT="$2"; shift 2 ;;
    --thread-count) THREAD_COUNT="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ -z "$HEDDLE_BIN" ]]; then
  HEDDLE_BIN="$ROOT/target/release/heddle"
fi
if [[ ! -x "$HEDDLE_BIN" ]]; then
  echo "error: heddle binary not executable: $HEDDLE_BIN" >&2
  echo "build with: cargo build --release -p heddle-cli --locked" >&2
  exit 1
fi
if ! [[ "$TRIALS" =~ ^[0-9]+$ ]] || [[ "$TRIALS" -lt 1 ]]; then
  echo "error: --trials must be integer >= 1" >&2
  exit 2
fi

mkdir -p "$OUT_DIR"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
COMMIT="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
BRANCH="$(git -C "$ROOT" branch --show-current 2>/dev/null || echo unknown)"

if [[ -n "$KEEP_FIXTURE" ]]; then
  FIXTURE="$KEEP_FIXTURE"
  mkdir -p "$FIXTURE"
  # Fresh equal-work tree each run when keeping a path
  find "$FIXTURE" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
else
  FIXTURE="$(mktemp -d "${TMPDIR:-/tmp}/heddle-core-loop-XXXXXX")"
  cleanup() { rm -rf "$FIXTURE"; }
  trap cleanup EXIT
fi

CONFIG_DIR="$FIXTURE/.heddle-user"
mkdir -p "$CONFIG_DIR"
# Isolate user config so ambient machine config cannot change work.
: >"$CONFIG_DIR/config.toml"
export HEDDLE_CONFIG="$CONFIG_DIR/config.toml"
# Accountable identity required for capture; fixed values keep the fixture equal-work.
export HEDDLE_PRINCIPAL_NAME="${HEDDLE_PRINCIPAL_NAME:-perf-baseline}"
export HEDDLE_PRINCIPAL_EMAIL="${HEDDLE_PRINCIPAL_EMAIL:-perf-baseline@heddle.local}"
unset HEDDLE_PROFILE || true
unset HEDDLE_AGENT_PROVIDER || true
unset HEDDLE_AGENT_MODEL || true

run_h() {
  (cd "$FIXTURE" && "$HEDDLE_BIN" "$@")
}

echo "==> building equal-work core-loop fixture at $FIXTURE" >&2
echo "    recipe: init + ${FILE_COUNT} files (20 dirs) + capture seed + ${THREAD_COUNT} threads + 1 dirty file" >&2

run_h init --principal-name "$HEDDLE_PRINCIPAL_NAME" --principal-email "$HEDDLE_PRINCIPAL_EMAIL"

# Match write_even_spread_files in perf_core_loop.rs exactly.
python3 - "$FIXTURE" "$FILE_COUNT" <<'PY'
import sys
from pathlib import Path

root = Path(sys.argv[1])
count = int(sys.argv[2])
for index in range(count):
    d = root / f"tracked-{index % 20:02d}"
    d.mkdir(parents=True, exist_ok=True)
    body = f"fixture file {index}\n{'x' * 80}\n"
    (d / f"file-{index:03d}.txt").write_text(body)
PY

run_h capture -m seed

for index in $(seq 0 $((THREAD_COUNT - 1))); do
  name=$(printf 'perf/thread-%02d' "$index")
  run_h thread create "$name" >/dev/null
done

# One dirty tracked file — same path and content as the Rust smoke fixture.
printf 'dirty\n' >"$FIXTURE/tracked-00/file-000.txt"

# Verify the fixture is live work (status / log / diff must succeed).
run_h --output json status >/dev/null
run_h --output json log >/dev/null
run_h --output json diff >/dev/null

echo "==> fixture ready (equal-work recipe locked)" >&2

# Operations mirror the core loop surface used for budgets (subset + key JSON ops).
# Names must stay stable for PERF_BASELINE.md aggregation.
OPS_JSON='[
  {"id": "bare_help", "args": []},
  {"id": "help", "args": ["help"]},
  {"id": "status_text", "args": ["status"]},
  {"id": "status_short", "args": ["status", "--short"]},
  {"id": "status_json", "args": ["--output", "json", "status"]},
  {"id": "log_json", "args": ["--output", "json", "log"]},
  {"id": "diff_json", "args": ["--output", "json", "diff"]},
  {"id": "thread_list_json", "args": ["--output", "json", "thread", "list"]}
]'

# Absolute multi-command timings via Python for truthful stats (median/mean/p95/p99/stdev).
ABS_OUT="$OUT_DIR/${STAMP}-core-loop-absolute.json"
python3 - "$HEDDLE_BIN" "$FIXTURE" "$HEDDLE_CONFIG" "$TRIALS" "$WARMUP" "$ABS_OUT" \
  "$COMMIT" "$BRANCH" "$STAMP" "$FILE_COUNT" "$THREAD_COUNT" "$OPS_JSON" <<'PY'
from __future__ import annotations

import json
import math
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path

heddle = sys.argv[1]
fixture = Path(sys.argv[2])
config = sys.argv[3]
trials = int(sys.argv[4])
warmup = int(sys.argv[5])
out_path = Path(sys.argv[6])
commit = sys.argv[7]
branch = sys.argv[8]
stamp = sys.argv[9]
file_count = int(sys.argv[10])
thread_count = int(sys.argv[11])
ops = json.loads(sys.argv[12])


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
    s = sorted(times)
    mean = statistics.fmean(times)
    med = statistics.median(times)
    stdev = statistics.stdev(times) if len(times) > 1 else 0.0
    return {
        "n": len(times),
        "mean_s": mean,
        "median_s": med,
        "stdev_s": stdev,
        "min_s": s[0],
        "max_s": s[-1],
        "p95_s": percentile(s, 95),
        "p99_s": percentile(s, 99),
        "mean_ms": mean * 1000.0,
        "median_ms": med * 1000.0,
        "stdev_ms": stdev * 1000.0,
        "min_ms": s[0] * 1000.0,
        "max_ms": s[-1] * 1000.0,
        "p95_ms": percentile(s, 95) * 1000.0,
        "p99_ms": percentile(s, 99) * 1000.0,
        "raw_s": times,
        "raw_ms": [t * 1000.0 for t in times],
    }


env = os.environ.copy()
env["HEDDLE_CONFIG"] = config
env.pop("HEDDLE_PROFILE", None)

results = []
for op in ops:
    args = list(op["args"])
    cmd = [heddle, *args]
    # Warmup (required success)
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
    raw_trials = []
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
        raw_trials.append({"trial": i, "seconds": elapsed, "ms": elapsed * 1000.0, "exit_code": 0})
    stats = summarize(times)
    results.append(
        {
            "id": op["id"],
            "argv": cmd,
            "stats": stats,
            "raw_trials": raw_trials,
        }
    )
    print(
        f"  {op['id']:<20} median={stats['median_ms']:.1f} ms  "
        f"p95={stats['p95_ms']:.1f} ms  mean={stats['mean_ms']:.1f} ms  "
        f"n={stats['n']}",
        file=sys.stderr,
    )

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
    "require_success": True,
    "operations": results,
    "disclaimer": (
        "Measurement calibration on a fixed equal-work fixture. "
        "Not a Git win claim. Absolute wall-clock process times only; "
        "no behavior skipped, no early-exit gaming."
    ),
}
out_path.parent.mkdir(parents=True, exist_ok=True)
out_path.write_text(json.dumps(payload, indent=2) + "\n")
print(f"wrote {out_path}", file=sys.stderr)
PY

if [[ "$RUN_PAIRED" -eq 1 ]]; then
  echo "==> paired-bench A==B absolute self-pairs (alternating thermal control)" >&2
  # Self-pair key ops so paired-bench path is exercised with the same fixture.
  for pair in \
    "status_json|--output json status" \
    "log_json|--output json log" \
    "diff_json|--output json diff" \
    "help|help"
  do
    id="${pair%%|*}"
    args="${pair#*|}"
    # shellcheck disable=SC2086
    cmd="\"$HEDDLE_BIN\" $args"
    python3 "$ROOT/scripts/program/paired-bench.py" \
      --name "core-loop-${id}" \
      --trials "$TRIALS" \
      --warmup "$WARMUP" \
      --cwd "$FIXTURE" \
      --out "$OUT_DIR/${STAMP}-core-loop-paired-${id}.json" \
      --a "$cmd" \
      --b "$cmd" \
      >/dev/null
  done
fi

# Machine environment snapshot next to the absolute results.
ENV_OUT="$OUT_DIR/${STAMP}-environment.txt"
{
  echo "timestamp_utc=$STAMP"
  echo "commit=$COMMIT"
  echo "branch=$BRANCH"
  echo "heddle_bin=$HEDDLE_BIN"
  echo "heddle_version=$("$HEDDLE_BIN" --version 2>/dev/null || true)"
  echo "uname=$(uname -a)"
  if command -v sw_vers >/dev/null 2>&1; then
    sw_vers
  fi
  if command -v sysctl >/dev/null 2>&1; then
    echo "cpu=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || true)"
    echo "mem_bytes=$(sysctl -n hw.memsize 2>/dev/null || true)"
  fi
  echo "rustc=$(rustc --version 2>/dev/null || true)"
  echo "cargo=$(cargo --version 2>/dev/null || true)"
  echo "python3=$(python3 --version 2>/dev/null || true)"
  echo "trials=$TRIALS"
  echo "warmup=$WARMUP"
  echo "file_count=$FILE_COUNT"
  echo "thread_count=$THREAD_COUNT"
  echo "absolute_json=$ABS_OUT"
} >"$ENV_OUT"

echo "==> done" >&2
echo "absolute: $ABS_OUT" >&2
echo "env:      $ENV_OUT" >&2
echo "fixture:  $FIXTURE${KEEP_FIXTURE:+ (kept)}" >&2
