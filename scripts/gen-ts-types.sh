#!/usr/bin/env bash
# Regenerate the TypeScript types + raw JSON Schemas the npm/Electron wrapper
# (#581/#584) consumes, from heddle's runtime schema introspection.
#
# Output (deterministic — a no-op diff means the contract is unchanged):
#   clients/npm/generated/heddle-schemas.ts    types + verb map + version pin
#   clients/npm/generated/heddle-schemas.json  raw JSON Schemas, keyed by verb
#
# CI can assert the checked-in files are in sync by running this and failing on
# a dirty tree.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out_dir="${1:-$repo_root/clients/npm/generated}"

cd "$repo_root"
cargo run --locked -p heddle-cli --example gen_ts_types \
  --features git-overlay,native,semantic,zstd \
  -- "$out_dir"
