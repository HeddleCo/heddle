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
# (default features, normal edges only)
# and FAILS if the live count exceeds baseline + slack. We persist only the
# count, not the full dep set, so on a regression the failure message points
# the author at `cargo tree` to find the new subtree.
#
# It uses `cargo tree` plus dependency-free workspace metadata — no crate build
# — so it is cheap enough to run on every PR. Selecting the CLI package keeps
# dev-only features from unrelated workspace targets out of the normal graph.
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

PACKAGE_NAME="$(
  python3 -c 'import json, sys; print(json.load(open(sys.argv[1])).get("package", "heddle-cli"))' \
    "$BASELINE_FILE"
)"

# Resolve the package-scoped normal graph from the workspace root so the active
# Cargo.lock is used. Full-workspace metadata unifies dev-only features from
# every member into shared dependencies and can count test server stacks in the
# production CLI closure.
TREE_FILE="$(mktemp)"
WORKSPACE_META_FILE="$(mktemp)"
trap 'rm -f "$TREE_FILE" "$WORKSPACE_META_FILE"' EXIT
(cd "$WORKSPACE_ROOT" && cargo tree --locked --package "$PACKAGE_NAME" \
  --edges normal --prefix none --format '{p}') >"$TREE_FILE"
(cd "$WORKSPACE_ROOT" && cargo metadata --format-version 1 --no-deps \
  --locked --quiet) >"$WORKSPACE_META_FILE"

# Single python pass: read the baseline JSON and package-scoped tree, compare
# the unique external crate names against baseline + slack, and exit non-zero
# on regression.
BASELINE_FILE="$BASELINE_FILE" TREE_FILE="$TREE_FILE" \
WORKSPACE_META_FILE="$WORKSPACE_META_FILE" python3 - <<'PY'
import json
import os
import sys

with open(os.environ["BASELINE_FILE"]) as f:
    base = json.load(f)

with open(os.environ["WORKSPACE_META_FILE"]) as f:
    workspace_meta = json.load(f)

pkg_name = base.get("package", "heddle-cli")
baseline = int(base["baseline"])
slack = int(base.get("slack", 0))
ceiling = baseline + slack

# `cargo tree --prefix none --format '{p}'` starts each line with the package
# name. Count names, not versions, to preserve the audit's metric.
workspace_names = {package["name"] for package in workspace_meta["packages"]}
with open(os.environ["TREE_FILE"]) as f:
    tree_names = {line.split(maxsplit=1)[0] for line in f if line.strip()}
external = tree_names - workspace_names
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
