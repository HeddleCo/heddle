#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Local checks only. Never invokes Wrangler, Docker, deployment or account APIs.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
out=${1:-"$root/prototypes/git-gateway/.demo/verify-$(date -u +%Y%m%dT%H%M%SZ)"}
if [[ -e "$out" ]]; then echo 'Choose a new evidence directory' >&2; exit 73; fi
mkdir -p "$out"
out=$(cd "$out" && pwd)
export HEDDLE_HOME="$out/test-home"
export PYTHONDONTWRITEBYTECODE=1
export PYTHONPATH="$root/prototypes/git-gateway"
export GATEWAY_NATIVE="${CARGO_TARGET_DIR:-$root/target}/debug/examples/gateway_native"
export GATEWAY_HOST="${CARGO_TARGET_DIR:-$root/target}/debug/examples/gateway_host"
export GATEWAY_AGENT_DEMO="${CARGO_TARGET_DIR:-$root/target}/debug/examples/gateway_agent_demo"
run() { local name=$1; shift; "$@" 2>&1 | tee "$out/$name.log"; }
run build cargo build --locked -p heddle-git-projection --examples
run rust cargo test --locked -p heddle-git-projection
run rust-publication cargo test --locked -p heddle-git-projection --features gateway-publication
run rust-host cargo test --locked -p heddle-git-projection --example gateway_host
run clippy cargo clippy --locked -p heddle-git-projection --features gateway-publication --all-targets -- -D warnings
run formatting rustfmt --edition 2024 --check crates/git-projection/src/gateway_view.rs crates/git-projection/src/gateway_write.rs \
  crates/git-projection/src/gateway_publication.rs \
  crates/git-projection/examples/gateway_native.rs crates/git-projection/examples/gateway_agent_demo.rs \
  crates/git-projection/examples/gateway_host.rs crates/git-projection/tests/gateway_visibility_security.rs \
  crates/git-projection/tests/gateway_history.rs crates/git-projection/tests/gateway_write.rs \
  crates/git-projection/tests/gateway_write_security.rs crates/git-projection/tests/gateway_preparation_security.rs \
  crates/git-projection/tests/gateway_publication.rs crates/git-projection/tests/gateway_publication_bridge.rs \
  crates/git-projection/tests/gateway_publication_security.rs crates/repo/src/thread_replication/reference_capture.rs
run python python3 -m unittest discover -s prototypes/git-gateway -v
run authorization-mutation python3 prototypes/git-gateway/check_authorization_guard.py
run packaging python3 -m unittest discover -s prototypes/git-gateway/container -v
run worker node --test prototypes/git-gateway/worker/*.test.mjs prototypes/git-gateway/worker/cloudflare/*.test.mjs
# Requires the documented npm ci in this directory. No automatic installation.
(cd prototypes/git-gateway/worker/cloudflare; run syntax npm run check; run bundle npm run bundle)
run smoke python3 -m gateway.smoke --binary "$GATEWAY_NATIVE" --out "$out/smoke"
printf 'LOCAL verification passed. Evidence: %s\nLive Cloudflare remains unverified.\n' "$out"
