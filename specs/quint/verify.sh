#!/usr/bin/env bash
# Run Quint simulation on all specs. No Java required.
#
# Usage:
#   ./specs/quint/verify.sh              # Quick (10K traces, ~5s)
#   ./specs/quint/verify.sh --thorough   # Thorough (500K traces, ~60s)

set -euo pipefail

SAMPLES=10000
STEPS=20

if [[ "${1:-}" == "--thorough" ]]; then
  SAMPLES=500000
  STEPS=50
fi

cd "$(dirname "$0")/../.."

failed=0
total=0
passed=0

for spec in specs/quint/*.qnt; do
  name="$(basename "$spec")"
  [ "$name" = "common.qnt" ] && continue
  total=$((total + 1))
  printf "%-30s " "$name"
  if output=$(quint run --max-samples="$SAMPLES" --max-steps="$STEPS" --invariant=safety "$spec" 2>&1); then
    rate=$(echo "$output" | grep -o '[0-9]* traces/second' | head -1)
    echo "PASS ($rate)"
    passed=$((passed + 1))
  else
    echo "FAIL"
    echo "$output" | tail -5
    failed=1
  fi
done

echo ""
echo "$passed/$total specs passed"

# Also run Rust property tests if cargo is available
if command -v cargo &>/dev/null; then
  echo ""
  echo "Running Rust property tests..."
  if cargo test --test formal_specs 2>&1 | tail -3; then
    echo "Property tests PASS"
  else
    echo "Property tests FAIL"
    failed=1
  fi
fi

exit $failed
