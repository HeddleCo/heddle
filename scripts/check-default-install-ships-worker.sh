#!/usr/bin/env bash
# Assert that the default `cargo install -p heddle-cli` flow ships
# `heddle-fuse-worker` alongside `heddle`. The runtime sibling lookup
# in `mount::worker::default_worker_binary` only resolves when the two
# binaries live in the same bin dir; if the worker is gated out of the
# default install (e.g. because the `mount` feature isn't in the
# `[features] default = [...]` set), Linux mounts silently fall back
# to NFS at runtime (heddle#190 r5 / Codex PR #225 P2).
#
# We resolve the target dir via `cargo metadata` (rather than piping
# `cargo build --message-format=json` through a reader that breaks
# early) so the check works regardless of `CARGO_TARGET_DIR` /
# `.cargo/config.toml` redirects, AND avoids the SIGPIPE+pipefail
# trap that bit r5–r7 on arm64 CI runners: cargo was still streaming
# JSON when the consumer hit its match and closed stdin, cargo got
# SIGPIPE (exit 141), and `set -o pipefail` propagated that.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

cargo build --locked -p heddle-cli >/dev/null

target_dir=$(cargo metadata --no-deps --format-version=1 \
    | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
worker="${target_dir}/debug/heddle-fuse-worker"

if [ ! -x "$worker" ]; then
    echo "ERROR: default \`cargo build -p heddle-cli\` did not produce heddle-fuse-worker at:" >&2
    echo "  $worker" >&2
    echo "  heddle-cli's [features] default = [...] must include \"mount\"" >&2
    echo "  (or the [[bin]] heddle-fuse-worker must drop required-features = [\"mount\"])." >&2
    exit 1
fi

echo "OK: heddle-fuse-worker built at $worker"
