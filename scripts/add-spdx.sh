#!/usr/bin/env bash
# Adds an SPDX-License-Identifier header to every .rs file under
# the OSS-bound crates. Idempotent — files that already have the
# header are left alone.
#
# Run from the repo root: `bash scripts/add-spdx.sh`
#
# Excluded: server/, hosted/, hosted-client/ (closed-bound, will live
# in the private heddle-hosted workspace post-split).

set -euo pipefail

HEADER="// SPDX-License-Identifier: Apache-2.0"

OSS_CRATES=(
  crates/objects
  crates/refs
  crates/oplog
  crates/repo
  crates/crypto
  crates/semantic
  crates/proto
  crates/grpc
  crates/ingest
  crates/mount
  crates/daemon
  crates/cli
  crates/cli-shared
  crates/hosted-client-shim
  crates/devtools
  crates/review
  crates/state_review
)

changed=0
for dir in "${OSS_CRATES[@]}"; do
  if [ ! -d "$dir" ]; then
    continue
  fi
  while IFS= read -r f; do
    if ! head -1 "$f" | grep -q "SPDX-License-Identifier"; then
      printf '%s\n%s' "$HEADER" "$(cat "$f")" > "$f.tmp"
      mv "$f.tmp" "$f"
      changed=$((changed + 1))
    fi
  done < <(find "$dir" -name '*.rs' -type f)
done

echo "SPDX headers added to $changed files"
