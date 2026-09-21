#!/usr/bin/env bash
# Resolve hosted leaf crates as one external consumer, without workspace feature unification.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
api_rev=$(python3 - "$repo_root/Cargo.toml" <<'PY'
import sys
import tomllib

with open(sys.argv[1], 'rb') as manifest:
    print(tomllib.load(manifest)['workspace']['dependencies']['api']['rev'])
PY
)
consumer_dir=$(mktemp -d)
trap 'rm -rf "$consumer_dir"' EXIT
mkdir -p "$consumer_dir/src"

cat > "$consumer_dir/Cargo.toml" <<EOF
[package]
name = "heddle-hosted-leaf-consumer-check"
version = "0.0.0"
edition = "2024"

[workspace]

[dependencies]
config = { package = "heddle-config", path = "$repo_root/crates/config", default-features = false }
crypto = { package = "heddle-crypto", path = "$repo_root/crates/crypto", features = ["owner-root"] }
objects = { package = "heddle-objects", path = "$repo_root/crates/objects", features = ["async-source"] }
semantic = { package = "heddle-semantic", path = "$repo_root/crates/semantic" }

[patch.crates-io]
heddle-api = { git = "https://github.com/HeddleCo/api.git", rev = "$api_rev" }
EOF

cat > "$consumer_dir/src/lib.rs" <<'EOF'
pub use config::UserConfig;
pub use crypto::owner_root::sign_spool_owner_genesis;
pub use objects::operation_dedup::{
    DEFAULT_RETENTION_SECS, DedupOutcome, hash_request_body,
};
pub use objects::thread_record::ThreadRecord;
pub use objects::transfer::is_ancestor_async;
pub use semantic::index_assembly::SemanticIndexBuilder;
EOF

cargo tree --manifest-path "$consumer_dir/Cargo.toml" --edges normal,build --prefix none \
    > "$consumer_dir/tree.txt"
if rg '^(heddle-repo|rusqlite|libsqlite3-sys|ureq|sley-worktree|sley-hooks|sley-sequencer) v' \
    "$consumer_dir/tree.txt"; then
    echo "Hosted leaf consumer includes a local repository, SQLite, HTTP client, or local Sley engine" >&2
    exit 1
fi
cargo check --manifest-path "$consumer_dir/Cargo.toml"
