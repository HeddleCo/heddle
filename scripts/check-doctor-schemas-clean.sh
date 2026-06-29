#!/usr/bin/env bash
# Assert `heddle doctor schemas` reports no drift against the committed
# `docs/json-schemas.md`. IMPROVEMENT_PLAN §7 ("Substrate-specific"
# gates) carries the DoD that `doctor schemas` must stay clean after any
# `Principal`/output-boundary change (heddle#593); without a CI gate a
# schema drift lands silently. heddle#605.
#
# `heddle doctor schemas` regenerates every registered schema mirror
# from the running binary and diffs each documented `--output json`
# sample in `docs/json-schemas.md` against it. On drift (a sample key
# the schema doesn't declare, a renamed/dropped field, a coverage gap)
# the command exits non-zero with a typed recovery envelope; on a clean
# tree it exits 0. So the gate is simply: build the binary from THIS
# checkout, run `doctor schemas`, and propagate its exit status. The
# committed baseline is `docs/json-schemas.md` itself.
#
# We build with `--message-format=json` and read the artifact's
# absolute `executable` path (mirroring
# `check-default-install-ships-worker.sh`) so the gate enumerates the
# binary THIS invocation produced — a stale `heddle` in a cached
# `target/` can't fool it — and so it works under cross-compilation
# (`--target`, `build.target` in `.cargo/config.toml`) where the binary
# lands under `${target_dir}/<triple>/debug/`. Writing the build log to
# disk instead of piping avoids the SIGPIPE+pipefail trap.
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

build_log=$(mktemp)
trap 'rm -f "$build_log"' EXIT

cargo build --locked -p heddle-cli --bin heddle --message-format=json >"$build_log"

heddle_bin=$(python3 -c '
import json, sys
with open(sys.argv[1]) as f:
    for line in f:
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if (m.get("reason") == "compiler-artifact"
                and m.get("target", {}).get("name") == "heddle"
                and m.get("executable")):
            print(m["executable"])
            sys.exit(0)
sys.exit(1)
' "$build_log") || {
    echo "ERROR: \`cargo build -p heddle-cli --bin heddle\` did not emit a heddle compiler-artifact with an executable." >&2
    exit 1
}

if [ ! -x "$heddle_bin" ]; then
    echo "ERROR: heddle artifact path is not executable: $heddle_bin" >&2
    exit 1
fi

# Run against this checkout. `--repo` pins the source root so the drift
# check reads THIS tree's `docs/json-schemas.md` rather than discovering
# some ambient repo. A non-zero exit means the documented samples
# drifted from the generated schemas (or a coverage gap opened); the
# command prints the offending verb/field and the recovery command.
echo "Running \`heddle doctor schemas\` against $repo_root ..."
if "$heddle_bin" --repo "$repo_root" doctor schemas; then
    echo "OK: doctor schemas reports no drift against docs/json-schemas.md"
else
    status=$?
    echo "ERROR: \`heddle doctor schemas\` reported schema/doc drift (exit $status)." >&2
    echo "  Update the schema mirror in crates/cli/src/cli/commands/schemas.rs and/or" >&2
    echo "  the documented sample in docs/json-schemas.md so they agree, then rerun." >&2
    echo "  \`heddle doctor schemas --update-docs\` refreshes the machine-contract coverage sample." >&2
    exit "$status"
fi
