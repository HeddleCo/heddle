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
#   bash scripts/program/core-loop-bench.sh --cert   # forces n>=30 for tail stats
#   bash scripts/program/core-loop-bench.sh --keep-fixture /tmp/heddle-core-loop-fixture
#
# Outputs:
#   artifacts/perf/<stamp>-core-loop-absolute.json   multi-op absolute timings
#   artifacts/perf/<stamp>-core-loop-<op>.json       optional per-op paired-bench (A==B)
#
# Default n=3 is smoke/calibration only. p95/p99 are null unless n>=30.
# A==B self-pairs are runner calibration, not performance evidence.
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
CERT=0
MIN_TAIL_N=30

usage() {
  cat <<'EOF'
Usage: core-loop-bench.sh [options]

  --heddle PATH       Path to heddle release binary (default: target/release/heddle)
  --trials N          Timed trials per operation (default: 3 smoke; use >=30 for cert)
  --cert              Certification mode: force trials>=30 if lower
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
    --cert) CERT=1; shift ;;
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

if [[ "$CERT" -eq 1 ]] && [[ "$TRIALS" -lt "$MIN_TAIL_N" ]]; then
  echo "note: --cert raising trials from $TRIALS to $MIN_TAIL_N" >&2
  TRIALS=$MIN_TAIL_N
fi

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

# Absolute multi-command timings (implementation lives in core_loop_absolute.py
# so smoke tests exercise the real summarizer/printer, not a duplicate).
ABS_OUT="$OUT_DIR/${STAMP}-core-loop-absolute.json"
python3 "$ROOT/scripts/program/core_loop_absolute.py" \
  --heddle "$HEDDLE_BIN" \
  --fixture "$FIXTURE" \
  --config "$HEDDLE_CONFIG" \
  --trials "$TRIALS" \
  --warmup "$WARMUP" \
  --out "$ABS_OUT" \
  --commit "$COMMIT" \
  --branch "$BRANCH" \
  --stamp "$STAMP" \
  --file-count "$FILE_COUNT" \
  --thread-count "$THREAD_COUNT" \
  --ops-json "$OPS_JSON"

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
