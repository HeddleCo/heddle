#!/usr/bin/env bash
# Mutation tests for scripts/check-no-silent-default-tree-load.sh.
#
# The asserter is itself load-bearing — if it silently passes when
# the bug class reappears, the production lockdown means nothing. The
# Codex r1 review of heddle#93 caught two bypasses that made the
# asserter trivially defeatable (file-level allowlist prefix match,
# missing --multiline on rg). This test exercises four fixtures that
# r1's asserter would have passed but r2's must reject:
#
#   1. Single-line `get_tree(x)?.unwrap_or_default()` in a
#      non-allowlisted file → fails.
#   2. Multi-line `get_tree(x)?\n    .unwrap_or_default()` → fails.
#      r1 would have passed this (no --multiline on rg).
#   3. A site at an exact `path:line` allowlist entry → passes.
#   4. Same allowlisted file but at a different line → fails.
#      r1 would have passed this (prefix-match allowlist).
#
# The asserter exposes two env knobs to make this driveable:
#   HEDDLE_ASSERTER_SEARCH_DIRS — colon-separated dirs to search
#                                (default: `crates`)
#   HEDDLE_ASSERTER_ALLOWLIST   — semicolon-separated `path:line`
#                                entries; replaces the default list
#                                (empty string disables the list)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ASSERTER="$SCRIPT_DIR/check-no-silent-default-tree-load.sh"

if [[ ! -x "$ASSERTER" && ! -f "$ASSERTER" ]]; then
  echo "asserter not found at $ASSERTER" >&2
  exit 1
fi

fail=0
fixtures_root="$(mktemp -d)"
trap 'rm -rf "$fixtures_root"' EXIT

run_case() {
  local name="$1"; shift
  local fixture_subdir="$1"; shift
  local allowlist="$1"; shift
  local expect="$1"; shift  # "pass" or "fail"

  local search_dir="$fixtures_root/$fixture_subdir"
  local rc=0
  HEDDLE_ASSERTER_SEARCH_DIRS="$search_dir" \
    HEDDLE_ASSERTER_ALLOWLIST="$allowlist" \
    bash "$ASSERTER" >/dev/null 2>&1 || rc=$?

  if [[ "$expect" == "pass" ]]; then
    if (( rc == 0 )); then
      echo "ok: $name (asserter passed as expected)"
    else
      echo "::error::$name expected pass but asserter exited $rc" >&2
      fail=1
    fi
  else
    if (( rc != 0 )); then
      echo "ok: $name (asserter failed as expected)"
    else
      echo "::error::$name expected failure but asserter exited 0" >&2
      fail=1
    fi
  fi
}

# --- Fixture 1: single-line bug shape, no allowlist ----------------------
mkdir -p "$fixtures_root/single/crates/foo/src"
cat > "$fixtures_root/single/crates/foo/src/lib.rs" <<'EOF'
fn load(repo: &Repository, h: &ContentHash) -> Tree {
    repo.store().get_tree(h)?.unwrap_or_default()
}
EOF
run_case \
  "single-line get_tree(x)?.unwrap_or_default() is rejected" \
  "single/crates" "" "fail"

# --- Fixture 2: multi-line bug shape, no allowlist -----------------------
# r1's asserter (no --multiline on rg) would have passed this. r2 must
# reject it — this is the regression Codex flagged.
mkdir -p "$fixtures_root/multi/crates/foo/src"
cat > "$fixtures_root/multi/crates/foo/src/lib.rs" <<'EOF'
fn load(repo: &Repository, h: &ContentHash) -> Tree {
    repo.store()
        .get_tree(h)?
        .unwrap_or_default()
}
EOF
run_case \
  "multi-line get_tree chain is rejected (r1 regression)" \
  "multi/crates" "" "fail"

# --- Fixture 3: allowlisted exactly at the bug's line ---------------------
# A site at the exact line in the allowlist must be exempt. We pin the
# bug to a known line by inserting a fixed-length preamble.
mkdir -p "$fixtures_root/exact/crates/foo/src"
{
  echo "// line 1"
  echo "// line 2"
  echo "fn load(repo: &Repository, h: &ContentHash) -> Tree {"
  echo "    repo.store().get_tree(h)?.unwrap_or_default()"
  echo "}"
} > "$fixtures_root/exact/crates/foo/src/lib.rs"
run_case \
  "exact path:line allowlist match exempts the site" \
  "exact/crates" \
  "$fixtures_root/exact/crates/foo/src/lib.rs:4" \
  "pass"

# --- Fixture 4: allowlisted file but the bug is at a different line -----
# r1's prefix-match allowlist would have passed this. r2 must reject:
# moving the bug to a new line in an allowlisted file is exactly the
# "future regression sneaks in under the prior justification" shape
# the asserter exists to prevent.
mkdir -p "$fixtures_root/wrongline/crates/foo/src"
{
  echo "// line 1"
  echo "// line 2"
  echo "// line 3 — old allowlisted home, now empty"
  echo "fn load(repo: &Repository, h: &ContentHash) -> Tree {"
  echo "    repo.store().get_tree(h)?.unwrap_or_default()"
  echo "}"
} > "$fixtures_root/wrongline/crates/foo/src/lib.rs"
run_case \
  "allowlist scoped to old line does NOT exempt new line (r1 regression)" \
  "wrongline/crates" \
  "$fixtures_root/wrongline/crates/foo/src/lib.rs:3" \
  "fail"

# --- Fixture 5: doc-comment is still filtered (no false positive) --------
mkdir -p "$fixtures_root/doccomment/crates/foo/src"
cat > "$fixtures_root/doccomment/crates/foo/src/lib.rs" <<'EOF'
/// The legacy pattern was `get_tree(...)?.unwrap_or_default()`,
/// which silently substituted Tree::default() for missing trees.
fn noop() {}
EOF
run_case \
  "doc-comment quoting the legacy pattern is not flagged" \
  "doccomment/crates" "" "pass"

if (( fail )); then
  echo "asserter self-tests FAILED" >&2
  exit 1
fi
echo "asserter self-tests passed"
