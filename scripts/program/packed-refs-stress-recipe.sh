#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Packed-refs scale stress recipe wrapper.
# Docs: docs/program/PACKED_REFS_STRESS.md
#
# Runs the checked-in Criterion bench that exercises packed-refs (product
# format) vs reftable prototype at 10k / 50k / 100k. Does NOT claim reftable
# is product-shipped. Does NOT register a CI oracle gate.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

FILTER=""
if [[ "${1:-}" == "--filter" && -n "${2:-}" ]]; then
  FILTER="$2"
  shift 2
elif [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  cat <<'EOF'
Usage: bash scripts/program/packed-refs-stress-recipe.sh [--filter <criterion_filter>]

Runs: cargo bench -p heddle-refs --bench reftable_vs_packed [-- <filter>]

Fixture sizes (in-bench): 10_000, 50_000, 100_000 thread refs.
Product path: text packed-refs (Shipped; degrades ~10k+).
Reftable branch of the bench: spike prototype only (Planned / not product).

See docs/program/PACKED_REFS_STRESS.md for expected graceful behavior and
explicit non-claims.
EOF
  exit 0
fi

echo "== packed-refs stress recipe =="
echo "repo: $ROOT"
echo "bench: heddle-refs / reftable_vs_packed"
echo "sizes: 10k / 50k / 100k (see crates/refs/benches/reftable_vs_packed.rs)"
echo ""
echo "Expected (product packed-refs):"
echo "  - Correct load/lookup/list/rewrite at all sizes"
echo "  - Latency degrades as N grows (especially cold_load, cold_single_lookup, append_one_persist)"
echo "  - Prefer slow over silent corruption; host may timeout 100k samples under tight resources"
echo ""
echo "Non-claims:"
echo "  - reftable prototype in this bench is NOT product RefManager backend"
echo "  - this run is NOT a Wave 7 full-green certificate"
echo "  - this run is NOT a continuous CI scale gate"
echo ""

CMD=(cargo bench -p heddle-refs --bench reftable_vs_packed)
if [[ -n "$FILTER" ]]; then
  CMD+=(-- "$FILTER")
  echo "filter: $FILTER"
fi

echo "running: ${CMD[*]}"
echo ""
exec "${CMD[@]}"
