#!/usr/bin/env bash
# Mutation tests for scripts/check-no-silent-default-tree-load.sh.
#
# The asserter is itself load-bearing — if it silently passes when
# the bug class reappears, the production lockdown means nothing. This
# test harness drives a synthetic fixture through the wrapper script
# and asserts the expected pass/fail verdict.
#
# Heddle#103 swapped the regex implementation for an AST walker
# (`heddle-devtools check-no-silent-default-tree-load`). The 9 fixtures
# below are the regression set inherited from the regex era — fixtures
# 1..9 each map to a known regex bypass that the AST walker must continue
# to reject. Fixtures 10..12 are the new AST-era set: shapes that the
# regex could not have caught (or false-positived on) regardless of how
# many edge cases were stitched on.
#
# Driving knobs (both wrapper and binary honour the same env vars):
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

# Pre-build the binary once so per-case invocations stay fast and the
# first cargo-build noise doesn't drown out the test output.
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
(
  cd "$WORKSPACE_ROOT"
  cargo build -p heddle-devtools --quiet
) >/dev/null

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
# r1's regex asserter (no --multiline on rg) passed this. AST walker
# doesn't care about line breaks — the chain is the chain.
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
# A site at the exact line in the allowlist must be exempt. The AST
# walker reports the line of the `unwrap_or_default` ident; the fixture
# pins that line via a fixed-length preamble.
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
# r1's prefix-match allowlist passed this. Moving the bug to a new line
# in an allowlisted file is exactly the "future regression sneaks in
# under the prior justification" shape the asserter exists to prevent.
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
# Doc-comments aren't `ExprMethodCall` nodes, so the AST walker can't
# see them — they're exempt by construction, not by a comment-stripping
# regex pass.
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
# r2's regex used `get_tree\([^)]*\)` — the `[^)]*` stops at the
# first close-paren, so any arg containing a nested call slipped past.
# AST inspection doesn't care about arg shape; `get_tree` is `get_tree`.
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
# `unwrap_or_else(|| { Tree::new() })` — the AST classifier recognizes
# both bare and block-bodied closures whose tail expression constructs
# a default Tree.
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
# The regex Option-chain matcher capped each hop's gap at 200 chars in
# r2, then 1000 in r3 — both arbitrary bounds. AST walks the receiver
# chain directly; gap length is irrelevant.
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
  "Option-chain with long inter-hop gap is rejected (r2 regression)" \
  "longgap/crates" "" "fail"

# --- Fixture 9: multi-line doc-comment quoting the legacy chain ----------
# r2's ML regex branch skipped the comment filter — any `///`-prefixed
# prose mentioning the chain fired as a violation. AST inspection
# doesn't see comments at all.
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

# --- Fixture 10: triple-nested parens (heddle#103 new) -------------------
# Would have bypassed any bounded-depth `[^)]*` arg matcher: r2's regex
# balanced exactly two levels, so three defeated it. The AST walker
# parses arbitrary expression depth as a single `Expr::Paren` chain
# inside the call's arg, and the method name `get_tree` is what we key
# on — not the arg shape.
mkdir -p "$fixtures_root/triplenested/crates/foo/src"
cat > "$fixtures_root/triplenested/crates/foo/src/lib.rs" <<'EOF'
fn load(repo: &Repository, id: ContentHash) -> Tree {
    repo.store().get_tree(((id)))?.unwrap_or_default()
}
EOF
run_case \
  "triple-nested parens get_tree(((id))) is rejected (heddle#103 new)" \
  "triplenested/crates" "" "fail"

# --- Fixture 11: macro-call wrapper (heddle#103 new) ---------------------
# Documents the limitation: `syn::parse_file` does not expand macros,
# so a `get_tree_macro!(h)?.unwrap_or_default()` chain has the macro
# call as its receiver root, not a `MethodCall` named `get_tree`. The
# AST walker punts (no flag). This is acceptable: macro-wrapped
# get_tree sites are rare and surface in code review; if one appears,
# the macro definition itself can be audited.
#
# Verifying the macro-call call site is NOT flagged:
mkdir -p "$fixtures_root/macrowrapped/crates/foo/src"
cat > "$fixtures_root/macrowrapped/crates/foo/src/lib.rs" <<'EOF'
macro_rules! get_tree_macro {
    ($x:expr) => { repo.store().get_tree($x) };
}

fn load(h: &ContentHash) -> Tree {
    get_tree_macro!(h)?.unwrap_or_default()
}
EOF
run_case \
  "macro-call wrapper is not flagged (documented limitation, heddle#103)" \
  "macrowrapped/crates" "" "pass"

# --- Fixture 12: raw-string literal embedding the bug shape (heddle#103 new)
# The regex would false-positive: the bytes `get_tree(x)?.unwrap_or_default()`
# inside an `r#"..."#` raw-string trip the pattern. AST inspection sees
# the literal as `Expr::Lit(LitStr)`, not as method calls — no flag.
mkdir -p "$fixtures_root/rawstring/crates/foo/src"
cat > "$fixtures_root/rawstring/crates/foo/src/lib.rs" <<'EOF'
fn describe() -> &'static str {
    let s = r#"get_tree(x)?.unwrap_or_default()"#;
    s
}
EOF
run_case \
  "raw-string literal containing the bug-shape text is not flagged (heddle#103 new)" \
  "rawstring/crates" "" "pass"

if (( fail )); then
  echo "asserter self-tests FAILED" >&2
  exit 1
fi
echo "asserter self-tests passed"
