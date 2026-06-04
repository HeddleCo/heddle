#!/usr/bin/env bash
# heddle#482 — authored-state signing-chokepoint guard.
#
# Every command that writes a *new authored state* must route through
# `Repository::put_authored_state` (which auto-signs before persisting), never
# call `store().put_state(...)` directly. A direct authored put bypasses
# signing and reopens the coverage-leak class (heddle#482 r2): `heddle fork`,
# `collapse`, `context set`, and the rebase replay paths each persisted an
# UNSIGNED state because signing lived at the call site instead of the
# repo-layer chokepoint. Lifting signing to the chokepoint closed the class;
# this asserter keeps it closed by failing CI the moment a NEW authored writer
# under the CLI command tree reaches `put_state` directly.
#
# Scope: `crates/cli/src/cli/commands` production code only. Test code builds
# unsigned fixtures on purpose, so we exempt:
#   - dedicated `tests.rs` files, and
#   - the trailing `#[cfg(test)]` module in any file (test mods live at the
#     file tail by convention).
# `put_state_serialized(...)` is NOT matched: it is the non-authored,
# signature-preserving replay/transfer API (sync, packfiles), which correctly
# carries an existing signature forward rather than minting a fresh one.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SCAN_DIR="$ROOT/crates/cli/src/cli/commands"

if [[ ! -d "$SCAN_DIR" ]]; then
  echo "ERROR: scan dir not found: $SCAN_DIR" >&2
  exit 2
fi

violations=0
while IFS= read -r -d '' file; do
  if [[ "$(basename "$file")" == "tests.rs" ]]; then
    continue
  fi
  # Emit `lineno: content` for production lines that call `.put_state(`,
  # stopping at the first `#[cfg(test)]` and skipping comment lines.
  hits="$(awk '
    /#\[cfg\(test\)\]/ { exit }
    /^[[:space:]]*\/\// { next }
    /\.put_state\(/ { printf "    %d: %s\n", FNR, $0 }
  ' "$file")"
  if [[ -n "$hits" ]]; then
    echo "ERROR: direct store().put_state for an authored state in: ${file#"$ROOT"/}"
    echo "$hits"
    echo "    -> route authored states through Repository::put_authored_state (heddle#482)"
    violations=$((violations + 1))
  fi
done < <(find "$SCAN_DIR" -name '*.rs' -type f -print0)

if [[ "$violations" -gt 0 ]]; then
  echo
  echo "Found ${violations} authored-state signing-chokepoint bypass(es)."
  echo "Each authored-state write must auto-sign via Repository::put_authored_state."
  exit 1
fi

echo "OK: no authored-state signing-chokepoint bypasses under ${SCAN_DIR#"$ROOT"/}"
