#!/usr/bin/env bash
# Assert that the default `cargo install -p heddle-cli` flow ships
# `heddle-fuse-worker` alongside `heddle`. The runtime sibling lookup
# in `mount::worker::default_worker_binary` only resolves when the two
# binaries live in the same bin dir; if the worker is gated out of the
# default install (e.g. because the `mount` feature isn't in the
# `[features] default = [...]` set), Linux mounts silently fall back
# to NFS at runtime (heddle#190 r5 / Codex PR #225 P2).
#
# We capture `cargo build --message-format=json` into a tempfile and
# parse it after cargo exits. This directly enumerates the artifacts
# THIS invocation produced (so stale binaries from a cached target/
# can't fool us — r8's file-existence check was defeated by exactly
# that), and reads the artifact's absolute `executable` path so it
# works under cross-compilation (`--target`, `CARGO_BUILD_TARGET`,
# `build.target` in `.cargo/config.toml`) where binaries land under
# `${target_dir}/<triple>/debug/`. Writing to disk instead of piping
# avoids the SIGPIPE+pipefail trap that bit r5–r7 on arm64 CI runners.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

build_log=$(mktemp)
trap 'rm -f "$build_log"' EXIT

cargo build --locked -p heddle-cli --message-format=json >"$build_log"

artifact=$(python3 -c '
import json, sys
with open(sys.argv[1]) as f:
    for line in f:
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if (m.get("reason") == "compiler-artifact"
                and m.get("target", {}).get("name") == "heddle-fuse-worker"
                and m.get("executable")):
            print(m["executable"])
            sys.exit(0)
sys.exit(1)
' "$build_log") || {
    echo "ERROR: default \`cargo build -p heddle-cli\` did not emit a heddle-fuse-worker compiler-artifact with an executable." >&2
    echo "  heddle-cli's [features] default = [...] must include \"mount\"" >&2
    echo "  (or the [[bin]] heddle-fuse-worker must drop required-features = [\"mount\"])." >&2
    exit 1
}

if [ ! -x "$artifact" ]; then
    echo "ERROR: heddle-fuse-worker artifact path is not executable: $artifact" >&2
    exit 1
fi

echo "OK: heddle-fuse-worker built at $artifact"
