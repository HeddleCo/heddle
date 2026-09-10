#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TARGET=${OWNER_AUTH_TARGET:-"$REPO_ROOT/target/owner-authorization-conformance"}
SCRATCH=${OWNER_AUTH_SCRATCH:-"$TARGET/corpus"}
BINDING_ROOT=${OWNER_AUTH_BINDING_ROOT:-"$REPO_ROOT/npm/dist"}
MANIFEST="$REPO_ROOT/owner-authorization-conformance/native-verifier/Cargo.toml"

if [[ ${OWNER_AUTH_BINDING_READY:-0} != 1 ]]; then
  npm run build --prefix "$REPO_ROOT"
fi

cargo build \
  --manifest-path "$MANIFEST" \
  --target-dir "$TARGET"

OWNER_AUTH_NATIVE_VERIFIER="$TARGET/debug/capability-verifier-conformance-native" \
OWNER_AUTH_BINDING_ROOT="$BINDING_ROOT" \
OWNER_AUTH_SCRATCH="$SCRATCH" \
node "$REPO_ROOT/owner-authorization-conformance/run.ts"
