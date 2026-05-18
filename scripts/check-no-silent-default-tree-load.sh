#!/usr/bin/env bash
# Asserter for the silent-corruption-on-missing-tree bug class closed
# by heddle#90 (merge_algo) and heddle#93 (presentation + mutation
# paths outside merge). Replaces the regex implementation that went
# three rounds of bypass-and-patch (heddle#103) with a syn-based AST
# walker living in `heddle-devtools`.
#
# The bug class:
#   repo.store().get_tree(&state.tree)?.unwrap_or_default()
# A missing subtree silently became `Tree::default()`, so merges
# erased subtrees with no conflict markers, status rendered empty
# content, `heddle clean --force` deleted tracked files against an
# empty baseline, etc. The fix is `Repository::require_tree(...)?`
# which surfaces a `MissingObject { object_type: "tree" }` error
# with a `heddle fsck --full` recovery hint.
#
# This wrapper exists for three reasons:
#   1. It's the contract the CI workflow + mutation-test harness
#      already use; keeping the entry point means neither needs to
#      learn the cargo invocation.
#   2. It pins the production allowlist in one place (DEFAULT_ALLOWLIST
#      below), so a refactor of the production sites doesn't require
#      touching Rust code.
#   3. It ensures the binary is built before invocation. CI without a
#      pre-build step would otherwise pay cargo-build latency on every
#      run and noisy-build-output would mix with asserter output.
#
# Driving knobs (consumed by both this wrapper and the binary):
#   HEDDLE_ASSERTER_SEARCH_DIRS — colon-separated dirs (default: `crates`)
#   HEDDLE_ASSERTER_ALLOWLIST   — semicolon-separated `path:line`
#                                 entries; replaces the default list
#                                 (empty string disables the list)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Default allowlist for production code. AST inspection naturally
# exempts doc-comments and string literals, so the legacy doc-comment
# pins (executor.rs:303/776, repository.rs:1577) are no longer needed.
#
# crates/mount/src/core.rs:2193 is a real heddle#90/#93 bug-class site
# that the legacy regex missed because the chain shape is
#   store.get_tree(&e.hash).map_err(MountError::Store)?.unwrap_or_default()
# i.e. the `?` is on `.map_err(...)`, not directly on `.get_tree(...)`,
# so the regex's `${GET_TREE_CALL}\?\s*\.unwrap_or_default` pattern
# never matched. The AST walker correctly flags it. Allowlisted here
# (not fixed) to keep heddle#103's scope on the asserter swap; the
# follow-up will replace it with a `require_tree`-style guard and
# remove this entry.
DEFAULT_ALLOWLIST="crates/mount/src/core.rs:2193"

# Honour the env override; otherwise apply the default.
if [[ -z "${HEDDLE_ASSERTER_ALLOWLIST+set}" ]]; then
  export HEDDLE_ASSERTER_ALLOWLIST="$DEFAULT_ALLOWLIST"
fi

# Build the binary up front so its first-run output doesn't interleave
# with the asserter's stdout. Quiet on success; cargo's own diagnostics
# still go through if the build fails.
(
  cd "$WORKSPACE_ROOT"
  cargo build -p heddle-devtools --quiet
)

exec cargo run -p heddle-devtools --quiet --manifest-path "$WORKSPACE_ROOT/Cargo.toml" -- \
  check-no-silent-default-tree-load
