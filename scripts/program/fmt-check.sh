#!/usr/bin/env bash
# Format check using nightly rustfmt so rustfmt.toml unstable options apply.
# Exit 0 = clean; exit 1 = drift; exit 2 = missing nightly rustfmt (caller may
# map to skip_prereq via prerequisites — this script still fails closed when
# invoked directly without the runner).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

if command -v rustup >/dev/null 2>&1 && rustup run nightly rustfmt --version >/dev/null 2>&1; then
  exec rustup run nightly cargo fmt --all -- --check
fi

if cargo +nightly fmt --version >/dev/null 2>&1; then
  exec cargo +nightly fmt --all -- --check
fi

echo "nightly rustfmt not available; install with: rustup toolchain install nightly --component rustfmt" >&2
echo "refusing to run stable cargo fmt (would mis-format imports_granularity/group_imports)" >&2
exit 2
