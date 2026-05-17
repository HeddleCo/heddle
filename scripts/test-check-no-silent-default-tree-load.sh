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

# --- Fixture 6: nested parens inside get_tree args (Codex r2 P2) ---------
# r2's asserter used `get_tree\([^)]*\)` — the `[^)]*` stops at the
# first close-paren, so any arg containing a nested call slipped past.
# r3 must catch this.
mkdir -p "$fixtures_root/nested/crates/foo/src"
cat > "$fixtures_root/nested/crates/foo/src/lib.rs" <<'EOF'
fn load(repo: &Repository, s: &State) -> Tree {
    repo.store().get_tree(&normalize(s.tree()))?.unwrap_or_default()
}
EOF
run_case \
  "nested-paren get_tree(&normalize(s.tree())) is rejected (r2 regression)" \
  "nested/crates" "" "fail"

# --- Fixture 7: braced closure body (Codex r2 P2) -----------------------
# `unwrap_or_else(|| { Tree::new() })` — the prior closure regex
# required a bare expression with no `{ ... }` around it.
mkdir -p "$fixtures_root/braced/crates/foo/src"
cat > "$fixtures_root/braced/crates/foo/src/lib.rs" <<'EOF'
fn load(repo: &Repository, h: &ContentHash) -> Tree {
    repo.store().get_tree(h)?.unwrap_or_else(|| { Tree::new() })
}
EOF
run_case \
  "braced-closure unwrap_or_else(|| { Tree::new() }) is rejected (r2 regression)" \
  "braced/crates" "" "fail"

# --- Fixture 8: long gap between Option-chain hops (Codex r2 P2) --------
# The Option-chain matcher capped each hop's gap at 200 chars in r2.
# A >200-char comment between `.transpose()?` and `.unwrap_or_default()`
# slipped through. r3 raises the cap to 1000/hop.
mkdir -p "$fixtures_root/longgap/crates/foo/src"
{
  echo "fn load(repo: &Repository, h: &ContentHash) -> Tree {"
  echo "    repo.store()"
  echo "        .get_tree(h)"
  echo "        .transpose()?"
  echo "        // $(printf 'x%.0s' {1..400})"
  echo "        .flatten()"
  echo "        .unwrap_or_default()"
  echo "}"
} > "$fixtures_root/longgap/crates/foo/src/lib.rs"
run_case \
  "Option-chain with >200-char gap between hops is rejected (r2 regression)" \
  "longgap/crates" "" "fail"

# --- Fixture 9: multi-line doc-comment quoting the legacy chain ----------
# r2's ML branch skipped the doc-comment filter entirely — any
# `///`-prefixed prose mentioning the .transpose/.flatten/.unwrap_or_default
# chain fired as a violation. r3 applies the same comment filter to
# both branches via process_hits.
mkdir -p "$fixtures_root/mldoccomment/crates/foo/src"
cat > "$fixtures_root/mldoccomment/crates/foo/src/lib.rs" <<'EOF'
/// Historic bug: the legacy code did
/// `repo.store().get_tree(h).transpose()?.flatten().unwrap_or_default()`
/// which silently substituted Tree::default() for a missing tree.
fn noop() {}
EOF
run_case \
  "multi-line doc-comment quoting the Option-chain is not flagged (r2 regression)" \
  "mldoccomment/crates" "" "pass"

if (( fail )); then
  echo "asserter self-tests FAILED" >&2
  exit 1
fi
echo "asserter self-tests passed"
