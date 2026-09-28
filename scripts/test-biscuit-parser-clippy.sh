#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
probe_dir=$(mktemp -d "${TMPDIR:-/tmp}/heddle-biscuit-clippy.XXXXXX")
trap 'rm -rf "$probe_dir"' EXIT
mkdir -p "$probe_dir/src"
cp "$repo_root/clippy.toml" "$probe_dir/clippy.toml"

cat > "$probe_dir/Cargo.toml" <<'TOML'
[package]
name = "biscuit-parser-clippy-probe"
version = "0.0.0"
edition = "2024"

[dependencies]
biscuit-auth = "=6.0.0"
TOML

cat > "$probe_dir/src/main.rs" <<'RUST'
use biscuit_auth::{Biscuit, KeyPair, UnverifiedBiscuit, datalog::SymbolTable, format::SerializedBiscuit};

fn main() {
    let key = KeyPair::new();
    let _ = Biscuit::unsafe_deprecated_deserialize(b"", key.public());
    let _ = UnverifiedBiscuit::from_with_symbols(b"", SymbolTable::new());
    let _ = UnverifiedBiscuit::from_base64_with_symbols(b"", SymbolTable::new());
    let _ = SerializedBiscuit::from_slice(b"", key.public());
    let _ = SerializedBiscuit::new;
}
RUST

if (cd "$probe_dir" && cargo clippy --quiet --offline -- -D warnings) > "$probe_dir/output" 2>&1; then
    echo "banned Biscuit parsers passed clippy -D warnings" >&2
    exit 1
fi

count=$(grep -c '^error: use of a disallowed method' "$probe_dir/output" || true)
if [[ "$count" != 5 ]]; then
    cat "$probe_dir/output" >&2
    echo "expected five disallowed-method diagnostics, got $count" >&2
    exit 1
fi
echo "five Biscuit constructor/parser probes rejected by clippy -D warnings"
