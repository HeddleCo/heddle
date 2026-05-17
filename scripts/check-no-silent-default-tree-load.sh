#!/usr/bin/env bash
# Asserter for the silent-corruption-on-missing-tree class of bug
# closed by heddle#90 (merge_algo) and heddle#93 (presentation +
# mutation paths outside merge).
#
# The original pattern was:
#   repo.store().get_tree(&state.tree)?.unwrap_or_default()
#
# A missing subtree silently became `Tree::default()`, so merges
# erased subtrees with no conflict markers, status rendered empty
# content, `heddle clean --force` deleted tracked files against an
# empty baseline, etc. The fix is `Repository::require_tree(...)?`
# which surfaces a `MissingObject { object_type: "tree" }` error
# with a `heddle fsck --full` recovery hint.
#
# This script fails CI if any of the following bug-class shapes
# reappear in production code:
#   - get_tree(...)?.unwrap_or_default()
#   - get_tree(...)?.unwrap_or_else(|| Tree::new())  / Tree::default()
#   - get_tree(...)?.unwrap_or_else(Tree::new)       / Tree::default
#   - get_tree(...).ok().flatten().unwrap_or_default()
#   - the .transpose()?.flatten().unwrap_or_default() Option-chain
#     variant when the chain originates from get_tree
#
# Doc-comments and tests (which legitimately reference the legacy
# pattern when describing the bug or asserting the migration) are
# whitelisted. The list of allowed-by-design lines lives below; add
# to it explicitly with a justification when a new legitimate
# sentinel appears.

set -euo pipefail

fail=0

err() { echo "::error::$*" >&2; fail=1; }
ok()  { echo "ok: $*"; }

# Search only production Rust source under crates/. Tests under
# crates/*/tests/ exercise the bug class explicitly and are exempt;
# the *_tests.rs files inside src/ are also exempt because they
# pin the migration's intended behavior. Comments inside
# executor.rs document the heddle#90 regression — also exempt.
SEARCH_DIRS=(crates)

# Patterns that ARE the bug. Each entry: pattern => description.
declare -a BUG_PATTERNS=(
  "get_tree\([^)]*\)\??\s*\.unwrap_or_default\(\)|.get_tree(...).unwrap_or_default()"
  "get_tree\([^)]*\)\??\s*\.unwrap_or_else\(\s*\|\|\s*Tree::(new|default)\(\)\s*\)|.get_tree(...).unwrap_or_else(|| Tree::...())"
  "get_tree\([^)]*\)\??\s*\.unwrap_or_else\(\s*Tree::(new|default)\s*\)|.get_tree(...).unwrap_or_else(Tree::...)"
)

# Multi-line Option-chain shape:
#   .map(|s| repo.store().get_tree(&s.tree))
#   .transpose()?
#   .flatten()
#   .unwrap_or_default()
# Detected with ripgrep multiline mode.

# Allowlist (file:line) — explicit exemptions. Anything matched
# here is reported as "exempt: <why>" instead of failing. Keep this
# list short; prefer migrating the site over allowlisting it.
declare -a ALLOWLIST=(
  # heddle#90 comments quoting the pre-fix pattern as part of the
  # documentation for the require_subtree helper.
  "crates/cli/src/cli/commands/merge/merge_algo/executor.rs:303"
  "crates/cli/src/cli/commands/merge/merge_algo/executor.rs:776"
  # heddle#93 doc-comment on Repository::require_tree quoting the
  # pre-fix pattern for context.
  "crates/repo/src/repository.rs"
)

is_allowed() {
  local fileline="$1"
  for entry in "${ALLOWLIST[@]}"; do
    if [[ "$fileline" == "$entry"* ]]; then
      return 0
    fi
  done
  return 1
}

run_rg() {
  local pattern="$1"
  local label="$2"
  local hits
  # --multiline so multi-line .transpose()?.flatten() chains are
  # caught. --type rust restricts to .rs.
  hits=$(rg --line-number --no-heading --type rust \
            "$pattern" "${SEARCH_DIRS[@]}" 2>/dev/null || true)
  if [[ -z "$hits" ]]; then
    ok "no occurrences: $label"
    return
  fi
  while IFS= read -r line; do
    # Skip empty lines from the loop.
    [[ -z "$line" ]] && continue
    # Strip the ripgrep `path:lineno:content` prefix to a `path:lineno` key.
    local key
    key="$(echo "$line" | awk -F: '{print $1":"$2}')"
    # Skip doc comments and inline comments — these legitimately
    # quote the legacy pattern when describing the bug class.
    local content
    content="$(echo "$line" | awk -F: '{$1=$2=""; sub(/^  /, ""); print}')"
    if echo "$content" | grep -Eq '^[[:space:]]*(///|//!|//|\*|/\*)' ; then
      continue
    fi
    if is_allowed "$key"; then
      ok "exempt: $key — $label"
      continue
    fi
    err "$label at $line"
  done <<< "$hits"
}

# Single-line shapes — straightforward ripgrep.
run_rg 'get_tree\([^)]*\)\?\.unwrap_or_default\(\)' \
       'silent-default tree load (heddle#90/#93 bug class)'
run_rg 'get_tree\([^)]*\)\?\s*\.unwrap_or_else\(\|\|\s*Tree::(new|default)\(\)\s*\)' \
       'silent-default tree load via unwrap_or_else(closure)'
run_rg 'get_tree\([^)]*\)\?\s*\.unwrap_or_else\(\s*Tree::(new|default)\s*\)' \
       'silent-default tree load via unwrap_or_else(fn-pointer)'
run_rg 'get_tree\([^)]*\)\.ok\(\)\.flatten\(\)\.unwrap_or_default\(\)' \
       'silent-default tree load via .ok().flatten().unwrap_or_default()'

# Multi-line Option-chain — must enable rg's multiline mode.
ML_HITS=$(rg --multiline --multiline-dotall --line-number --no-heading --type rust \
             '\.get_tree\([^)]*\)[\s\S]{0,200}\.transpose\(\)\?[\s\S]{0,200}\.flatten\(\)[\s\S]{0,200}\.unwrap_or_default\(\)' \
             "${SEARCH_DIRS[@]}" 2>/dev/null || true)
if [[ -z "$ML_HITS" ]]; then
  ok "no occurrences: Option-chain .transpose()?.flatten().unwrap_or_default() of get_tree"
else
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    key="$(echo "$line" | awk -F: '{print $1":"$2}')"
    if is_allowed "$key"; then
      ok "exempt: $key — Option-chain"
      continue
    fi
    err "Option-chain silent-default tree load at $line"
  done <<< "$ML_HITS"
fi

if [[ "$fail" -ne 0 ]]; then
  cat >&2 <<'EOF'

::error::Found one or more `get_tree(...)?.unwrap_or_default()`-class
sites in production code. This pattern silently substitutes
`Tree::default()` for a missing subtree, which is the silent-
corruption bug class closed by heddle#90 (merge) and heddle#93
(non-merge sweep). Replace with `repo.require_tree(&hash)?` so
missing trees surface a clear error with a `heddle fsck --full`
recovery hint.

If a site is a legitimate empty-tree sentinel (no-parent-commit
marker, etc.) add it to the ALLOWLIST in this script with a
one-line justification.
EOF
  exit 1
fi

echo "asserter clean: no silent-default tree load sites in production code"
