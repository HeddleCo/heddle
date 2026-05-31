#!/usr/bin/env bash
# Asserter for the cross-crate publish-first snapshot bug class
# (heddle#354 r8). A `syn`-based AST walker living in `heddle-devtools`
# walks the WHOLE workspace and fails CI if any production function
# co-locates a raw refs-publish (`refs.set_thread` / `refs.write_head`
# / etc.) with a snapshot-record append (`oplog.record_snapshot` or the
# `record_snapshot_in_oplog` wrapper).
#
# The bug class:
#   self.refs.set_thread(&thread, &state.change_id)?;  // PUBLISH (phase 5)
#   self.oplog.record_snapshot(...)?;                   // RECORD  (phase 4)
# A snapshot published its ref BEFORE recording its `OpRecord::Snapshot`.
# Because the reconciler folds a `Snapshot` record authoritatively
# (newest committed record wins), a late snapshot record carrying a
# stale thread value could clobber a newer concurrent write. The fix is
# `Repository::commit_snapshot_atomic(...)`, which commits the record
# BEFORE publishing the ref via the record-first `commit_and_publish`
# chokepoint.
#
# This complements the refs-crate-internal `write_read_conformance`
# check (heddle#354 r7): that one guards the refs-internal raw writers
# but is blind to cross-crate callers. This one walks every crate, so a
# publish-first snapshot in `repo`, `mount`, or any future caller fails
# CI (the coverage hole r7 left).
#
# Driving knobs (consumed by the binary):
#   HEDDLE_SNAPSHOT_ATOMICITY_SEARCH_DIRS — colon-separated dirs
#                                           (default: `crates`)
#   HEDDLE_SNAPSHOT_ATOMICITY_ALLOWLIST   — semicolon-separated
#                                           `path:line` entries (of the
#                                           publish call); empty string
#                                           disables, unset uses the
#                                           built-in default.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Default allowlist for production code: EMPTY. The two known sites
# (crates/repo/src/repository_snapshot.rs, crates/mount/src/core.rs)
# were fixed to route through `commit_snapshot_atomic`, not exempted.
DEFAULT_ALLOWLIST=""

if [[ -z "${HEDDLE_SNAPSHOT_ATOMICITY_ALLOWLIST+set}" ]]; then
  export HEDDLE_SNAPSHOT_ATOMICITY_ALLOWLIST="$DEFAULT_ALLOWLIST"
fi

# Build the binary up front so its first-run output doesn't interleave
# with the asserter's stdout. Quiet on success; cargo's own diagnostics
# still go through if the build fails.
(
  cd "$WORKSPACE_ROOT"
  cargo build -p heddle-devtools --quiet
)

exec cargo run -p heddle-devtools --quiet --manifest-path "$WORKSPACE_ROOT/Cargo.toml" -- \
  check-snapshot-atomicity
