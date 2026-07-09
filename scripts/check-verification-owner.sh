#!/usr/bin/env bash
# Asserter for Track A verification ownership (docs/VERIFICATION_CLEANUP_PLAN.md).
# Repository Verification State / health proof construction is owned by
# heddle-core. This wrapper keeps a stable CI entrypoint and pins the
# allowlist outside the Rust binary.
#
# Driving knobs:
#   HEDDLE_ASSERTER_SEARCH_DIRS           — colon-separated dirs (default: crates)
#   HEDDLE_VERIFICATION_OWNER_ALLOWLIST   — semicolon-separated path:line entries

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

DEFAULT_ALLOWLIST=""

if [[ -z "${HEDDLE_VERIFICATION_OWNER_ALLOWLIST+set}" ]]; then
  export HEDDLE_VERIFICATION_OWNER_ALLOWLIST="$DEFAULT_ALLOWLIST"
fi

(
  cd "$WORKSPACE_ROOT"
  cargo build -p heddle-devtools --quiet
)

exec cargo run -p heddle-devtools --quiet --manifest-path "$WORKSPACE_ROOT/Cargo.toml" -- \
  check-verification-owner
