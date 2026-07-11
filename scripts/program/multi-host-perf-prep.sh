#!/usr/bin/env bash
# Multi-host equal-work perf prep helper (Wave 6 residual).
#
# Writes a host card and prints the exact core-loop-bench command for this
# machine. Does NOT invent timings. Does NOT claim multi-host cert.
#
# Usage:
#   bash scripts/program/multi-host-perf-prep.sh
#   bash scripts/program/multi-host-perf-prep.sh --build
#   bash scripts/program/multi-host-perf-prep.sh --build --run
#
# See docs/program/MULTI_HOST_PERF.md

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

DO_BUILD=0
DO_RUN=0
TRIALS=5
WARMUP=1
OUT_DIR="${OUT_DIR:-$ROOT/artifacts/perf}"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/heddle-mh-perf-${USER}-$(uname -s)-$(uname -m)}"

usage() {
  cat <<'EOF'
Usage: multi-host-perf-prep.sh [options]

  --build         cargo build --release -p heddle-cli --features client
  --run           run core-loop-bench.sh (implies artifacts under OUT_DIR)
  --trials N      timed trials (default 5)
  --warmup N      warmup rounds (default 1)
  --out-dir DIR   artifact directory (default artifacts/perf)
  -h, --help      show help

Environment:
  CARGO_TARGET_DIR   release target (default /tmp/heddle-mh-perf-$USER-OS-ARCH)
  OUT_DIR            same as --out-dir
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --build) DO_BUILD=1; shift ;;
    --run) DO_RUN=1; shift ;;
    --trials) TRIALS="$2"; shift 2 ;;
    --warmup) WARMUP="$2"; shift 2 ;;
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

HOST_ID="$(hostname -s 2>/dev/null || hostname || echo unknown-host)"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
COMMIT="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
BRANCH="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
mkdir -p "$OUT_DIR"

HOST_CARD="$OUT_DIR/${STAMP}-${HOST_ID}-host-card.txt"
{
  echo "host_id=${HOST_ID}"
  echo "timestamp_utc=${STAMP}"
  echo "commit=${COMMIT}"
  echo "branch=${BRANCH}"
  echo "uname=$(uname -a)"
  if command -v rustc >/dev/null 2>&1; then echo "rustc=$(rustc --version)"; fi
  if command -v cargo >/dev/null 2>&1; then echo "cargo=$(cargo --version)"; fi
  if [[ "$(uname -s)" == "Darwin" ]]; then
    echo "os_product=$(sw_vers -productVersion 2>/dev/null || true)"
    echo "cpu=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || true)"
    echo "mem_bytes=$(sysctl -n hw.memsize 2>/dev/null || true)"
  else
    echo "cpu=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | xargs || true)"
    echo "mem_kb=$(awk '/MemTotal/ {print $2}' /proc/meminfo 2>/dev/null || true)"
  fi
  echo "loadavg=$( { uptime 2>/dev/null || true; } )"
  echo "cargo_target_dir=${TARGET_DIR}"
  echo "notes=Fill quiet/noisy and any concurrent jobs before citing externally."
} >"$HOST_CARD"

echo "==> wrote host card: $HOST_CARD"
echo "    commit=$COMMIT host=$HOST_ID"
echo
echo "Docs: docs/program/MULTI_HOST_PERF.md"
echo "Matrix: docs/program/MULTI_HOST_PERF_MATRIX.md"
echo
echo "Next (or re-run with --build / --run):"
echo "  export CARGO_TARGET_DIR=$TARGET_DIR"
echo "  cargo build --release -p heddle-cli --locked --features client"
echo "  bash scripts/program/core-loop-bench.sh \\"
echo "    --heddle $TARGET_DIR/release/heddle \\"
echo "    --trials $TRIALS --warmup $WARMUP \\"
echo "    --out-dir $OUT_DIR"
echo
echo "Multi-host residual stays OPEN until ≥2 hosts complete the recipe on the same commit."

if [[ "$DO_BUILD" -eq 1 ]]; then
  export CARGO_TARGET_DIR="$TARGET_DIR"
  echo "==> building release into $CARGO_TARGET_DIR"
  cargo build --release -p heddle-cli --locked --features client
fi

if [[ "$DO_RUN" -eq 1 ]]; then
  export CARGO_TARGET_DIR="$TARGET_DIR"
  HEDDLE_BIN="$TARGET_DIR/release/heddle"
  if [[ ! -x "$HEDDLE_BIN" ]]; then
    echo "error: missing binary $HEDDLE_BIN (pass --build first)" >&2
    exit 1
  fi
  echo "==> running core-loop-bench (n=$TRIALS)"
  bash "$ROOT/scripts/program/core-loop-bench.sh" \
    --heddle "$HEDDLE_BIN" \
    --trials "$TRIALS" \
    --warmup "$WARMUP" \
    --out-dir "$OUT_DIR"
fi
