#!/usr/bin/env bash
# Assert that the default `cargo install -p heddle-cli` flow ships
# `heddle-fuse-worker` alongside `heddle`. The runtime sibling lookup
# in `mount::worker::default_worker_binary` only resolves when the two
# binaries live in the same bin dir; if the worker is gated out of the
# default install (e.g. because the `mount` feature isn't in the
# `[features] default = [...]` set), Linux mounts silently fall back
# to NFS at runtime (heddle#190 r5 / Codex PR #225 P2).
#
# We use `cargo build --message-format=json` rather than poking at
# `target/debug/` directly so the check works regardless of
# `CARGO_TARGET_DIR` (the orchestrator points workspaces at a shared
# per-repo target dir) and regardless of build profile.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

artifact=$(cargo build --locked -p heddle-cli --message-format=json 2>/dev/null \
  | python3 -c '
import json, sys
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if (m.get("reason") == "compiler-artifact"
            and m.get("target", {}).get("name") == "heddle-fuse-worker"
            and m.get("executable")):
        print(m["executable"])
        break
')

if [ -z "$artifact" ]; then
    echo "ERROR: default \`cargo build -p heddle-cli\` did not produce heddle-fuse-worker" >&2
    echo "  heddle-cli's [features] default = [...] must include \"mount\"" >&2
    echo "  (or the [[bin]] must drop its required-features = [\"mount\"] gate)." >&2
    exit 1
fi

if [ ! -x "$artifact" ]; then
    echo "ERROR: artifact path is not executable: $artifact" >&2
    exit 1
fi

echo "OK: heddle-fuse-worker built at $artifact"
