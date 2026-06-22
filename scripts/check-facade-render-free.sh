#!/usr/bin/env bash
# Enforce the embeddable facade/domain crates stay free of render dependencies.

set -euo pipefail

manifests=(
  crates/core/Cargo.toml
  crates/repo/Cargo.toml
  crates/objects/Cargo.toml
  crates/merge/Cargo.toml
  crates/semantic/Cargo.toml
  crates/refs/Cargo.toml
  crates/oplog/Cargo.toml
  # NOTE: crates/ingest is intentionally NOT gated here. Its `clap` dep is for the
  # `src/bin/` importer tool, not the library surface (the lib is render-free).
  # Making that clap dep bin-only / feature-gated so the facade dep-tree is fully
  # clap-free is a tracked follow-up, not part of this scaffolding.
  crates/format/Cargo.toml
  crates/wire/Cargo.toml
  crates/crypto/Cargo.toml
)

forbidden='clap|anstyle|anstream|indicatif|console|termcolor|owo-colors'
fail=0

for manifest in "${manifests[@]}"; do
  if [[ ! -f "$manifest" ]]; then
    echo "::error::missing manifest checked by facade render-free gate: $manifest" >&2
    fail=1
    continue
  fi

  while IFS= read -r line; do
    line_without_comment="${line%%#*}"
    if [[ "$line_without_comment" =~ ^[[:space:]]*($forbidden)([[:space:]]*=|[[:space:]]*\.) ]]; then
      dep="${BASH_REMATCH[1]}"
      echo "::error file=${manifest}::forbidden render dependency '${dep}' listed" >&2
      fail=1
    fi
  done < "$manifest"
done

if (( fail )); then
  echo "facade render-free check FAILED" >&2
  exit 1
fi

echo "facade render-free check passed"
