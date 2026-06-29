#!/usr/bin/env bash
# Guard for the CLI transitive-dependency count (heddle#604).
#
# IMPROVEMENT_PLAN section 8 (P4) removed gix, a ~277-transitive-dep git
# library subtree, taking the `heddle-cli` crate from 485 transitive deps
# (docs/CLI_DEP_AUDIT_2026-05-12.md) down to the baseline recorded in
# scripts/cli-dep-count-baseline.json. Without a guard that number silently
# regrows: a careless `cargo add`, a feature flip that unifies a heavy
# subtree, or a gix-style library sneaking back in all add transitive deps
# that nobody notices until a cold build is slow again.
#
# This script counts the `heddle-cli` crate's transitive dependency closure
# (default features, normal edges only — the same method the audit doc uses)
# and FAILS if the live count exceeds baseline + slack. We persist only the
# count, not the full dep set, so on a regression the failure message points
# the author at `cargo tree` to find the new subtree.
#
# It uses `cargo metadata` only — no crate build — so it is cheap enough to
# run on every PR.
#
# Knobs:
#   HEDDLE_CLI_DEP_BASELINE_FILE — path to the baseline JSON
#                                  (default: scripts/cli-dep-count-baseline.json)
#
# To intentionally raise the ceiling (a deliberate dep addition): bump
# `baseline` in the JSON in the same PR, with a one-line justification in the
# PR body. Lowering it after a reduction is encouraged — it tightens the gate.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BASELINE_FILE="${HEDDLE_CLI_DEP_BASELINE_FILE:-$SCRIPT_DIR/cli-dep-count-baseline.json}"

if [[ ! -f "$BASELINE_FILE" ]]; then
  echo "error: baseline file not found: $BASELINE_FILE" >&2
  exit 1
fi

# Resolve the dependency graph from the workspace root so the active
# Cargo.lock is used. Write to a temp file (not a pipe) because the python
# below is itself fed on stdin via a heredoc — stdin is not available for the
# metadata payload.
META_FILE="$(mktemp)"
trap 'rm -f "$META_FILE"' EXIT
(cd "$WORKSPACE_ROOT" && cargo metadata --format-version 1 --quiet) >"$META_FILE"

# Single python pass: read the baseline JSON, walk the cli closure over normal
# edges (default features), compare against baseline + slack. Exit non-zero on
# regression. Keeps the graph-walk identical to docs/CLI_DEP_AUDIT_2026-05-12.md.
BASELINE_FILE="$BASELINE_FILE" META_FILE="$META_FILE" python3 - <<'PY'
import json
import os
import sys

with open(os.environ["META_FILE"]) as f:
    meta = json.load(f)

with open(os.environ["BASELINE_FILE"]) as f:
    base = json.load(f)

pkg_name = base.get("package", "heddle-cli")
baseline = int(base["baseline"])
slack = int(base.get("slack", 0))
ceiling = baseline + slack

# Workspace members (source is None) are not counted as external deps.
ws = {p["name"] for p in meta["packages"] if p["source"] is None}
node_by_id = {n["id"]: n for n in meta["resolve"]["nodes"]}
pkg_by_id = {p["id"]: p for p in meta["packages"]}

try:
    cli_id = next(
        p["id"]
        for p in meta["packages"]
        if p["name"] == pkg_name and p["source"] is None
    )
except StopIteration:
    print(f"error: workspace package {pkg_name!r} not found in cargo metadata", file=sys.stderr)
    sys.exit(1)

# Reachable closure over normal (non-dev, non-build) edges only.
seen, stack = set(), [cli_id]
while stack:
    nid = stack.pop()
    if nid in seen:
        continue
    seen.add(nid)
    for d in node_by_id[nid]["deps"]:
        # dep_kinds entry with kind == None is a normal (runtime) edge.
        if not any(k.get("kind") is None for k in d.get("dep_kinds", [])):
            continue
        stack.append(d["pkg"])

external = {pkg_by_id[i]["name"] for i in seen if pkg_by_id[i]["name"] not in ws}
count = len(external)

print(f"{pkg_name} transitive deps (default features): {count}")
print(f"baseline: {baseline}  slack: {slack}  ceiling: {ceiling}")

if count > ceiling:
    over = count - baseline
    print("", file=sys.stderr)
    print(
        f"FAIL: {pkg_name} transitive dep count {count} exceeds "
        f"baseline {baseline} + slack {slack} = {ceiling} (over by {over}).",
        file=sys.stderr,
    )
    print(
        "      A dependency subtree grew. Find the new crate(s) with:",
        file=sys.stderr,
    )
    print(f"        cargo tree -p {pkg_name} --edges normal", file=sys.stderr)
    print(
        "      If the addition is intentional, raise `baseline` in "
        f"{os.path.basename(os.environ['BASELINE_FILE'])} in this PR with a "
        "justification.",
        file=sys.stderr,
    )
    sys.exit(1)

if count < baseline:
    print(
        f"note: count {count} is below baseline {baseline}; consider lowering "
        "the baseline to keep the gate tight.",
    )

print("OK: within ceiling.")
PY
