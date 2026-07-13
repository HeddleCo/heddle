#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HEDDLE_BIN="${HEDDLE_BIN:-$ROOT/target/debug/heddle}"
HEDDLE_RUNTIME_PATH="${HEDDLE_RUNTIME_PATH-}"
HEDDLE_REMOTE_PROOF_KEY_PEM="${HEDDLE_REMOTE_PROOF_KEY_PEM-}"
HEDDLE_REMOTE_PROOF_KEY_PEM_PATH="${HEDDLE_REMOTE_PROOF_KEY_PEM_PATH-}"
WEFT_ADDR="${WEFT_ADDR:-}"
HEDDLE_SMOKE_REMOTE="${HEDDLE_SMOKE_REMOTE:-}"
ARTIFACT_ROOT="${HEDDLE_SMOKE_ARTIFACT_ROOT:-$ROOT/target/smoke-hosted-release}"
WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/heddle-hosted-smoke.XXXXXX")"
RUN_ID="$(date +%Y%m%d%H%M%S)-$$"

export HEDDLE_PRINCIPAL_NAME="${HEDDLE_PRINCIPAL_NAME:-Release Smoke}"
export HEDDLE_PRINCIPAL_EMAIL="${HEDDLE_PRINCIPAL_EMAIL:-release-smoke@example.com}"

if [[ "${HEDDLE_SMOKE_KEEP_WORK:-0}" != "1" ]]; then
  trap 'rm -rf "$WORK_ROOT"' EXIT
else
  echo "keeping smoke work root: $WORK_ROOT"
fi

mkdir -p "$ARTIFACT_ROOT"
find "$ARTIFACT_ROOT" -mindepth 1 -maxdepth 1 -exec rm -rf {} +

fail() {
  echo "error: $*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required"
}

require_command python3
[[ -x "$HEDDLE_BIN" ]] || fail "HEDDLE_BIN is not executable: $HEDDLE_BIN"
[[ -n "${HEDDLE_REMOTE_TOKEN:-}" ]] || fail "HEDDLE_REMOTE_TOKEN is required"

if [[ -z "${HEDDLE_CONFIG:-}" && ( -n "$HEDDLE_REMOTE_PROOF_KEY_PEM" || -n "$HEDDLE_REMOTE_PROOF_KEY_PEM_PATH" ) ]]; then
  cfg_dir="$WORK_ROOT/user-config"
  mkdir -p "$cfg_dir"
  if [[ -n "$HEDDLE_REMOTE_PROOF_KEY_PEM" ]]; then
    HEDDLE_REMOTE_PROOF_KEY_PEM_PATH="$cfg_dir/auth-proof-key.pem"
    printf '%s\n' "$HEDDLE_REMOTE_PROOF_KEY_PEM" > "$HEDDLE_REMOTE_PROOF_KEY_PEM_PATH"
    chmod 600 "$HEDDLE_REMOTE_PROOF_KEY_PEM_PATH"
  fi
  cat > "$cfg_dir/config.toml" <<EOF
[remote]
auth_proof_key_pem_path = '$HEDDLE_REMOTE_PROOF_KEY_PEM_PATH'
EOF
  chmod 600 "$cfg_dir/config.toml"
  export HEDDLE_CONFIG="$cfg_dir/config.toml"
fi

if [[ -z "$HEDDLE_SMOKE_REMOTE" ]]; then
  [[ -n "$WEFT_ADDR" ]] || fail "set WEFT_ADDR or HEDDLE_SMOKE_REMOTE"
  HEDDLE_SMOKE_REMOTE="heddle://$WEFT_ADDR"
fi

heddle_runtime() {
  if [[ -n "$HEDDLE_RUNTIME_PATH" ]]; then
    env PATH="$HEDDLE_RUNTIME_PATH" "$HEDDLE_BIN" "$@"
  else
    "$HEDDLE_BIN" "$@"
  fi
}

run_json() {
  local repo="$1"
  local label="$2"
  shift 2
  echo "==> $label: heddle $*"
  (
    cd "$repo"
    heddle_runtime --output json "$@"
  ) > "$ARTIFACT_ROOT/$label.json"
}

json_get_origin_url() {
  python3 - "$1" <<'PYJSON'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as f:
    data = json.load(f)

for remote in data.get("remotes", []):
    if remote.get("name") == "origin":
        print(remote.get("url", ""))
        raise SystemExit(0)
raise SystemExit("origin remote not found")
PYJSON
}

json_assert_nonempty_items() {
  python3 - "$1" "$2" <<'PYJSON'
import json
import sys

path, label = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as f:
    data = json.load(f)
items = data.get("items")
if items is None:
    items = data.get("discussions")
if not isinstance(items, list) or not items:
    raise SystemExit(f"{label} did not contain non-empty items: {data!r}")
PYJSON
}

json_get_discussion_id() {
  python3 - "$1" <<'PYJSON'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as f:
    data = json.load(f)
discussion_id = data.get("id")
if not discussion_id:
    raise SystemExit(f"discussion id missing: {data!r}")
print(discussion_id)
PYJSON
}

configure_git_identity() {
  local repo="$1"
  git -C "$repo" config user.name "$HEDDLE_PRINCIPAL_NAME"
  git -C "$repo" config user.email "$HEDDLE_PRINCIPAL_EMAIL"
}

native_flow() {
  local repo="$WORK_ROOT/native-$RUN_ID"
  local clone="$WORK_ROOT/native-clone-$RUN_ID"
  mkdir -p "$repo/src"

  (
    cd "$repo"
    heddle_runtime init \
      --principal-name "$HEDDLE_PRINCIPAL_NAME" \
      --principal-email "$HEDDLE_PRINCIPAL_EMAIL" >/dev/null
    printf '# Hosted Native Smoke\n\n' > README.md
    printf "pub fn run() -> &'static str { \"native smoke\" }\n" > src/lib.rs
    heddle_runtime capture -m "hosted native smoke seed" >/dev/null
    heddle_runtime context set \
      --path src/lib.rs \
      --scope symbol:run \
      --kind rationale \
      -m "run is the hosted native smoke entry point" >/dev/null
  )

  run_json "$repo" "native.discuss-open" discuss open src/lib.rs run "should this sync through hosted storage?"
  local discussion_id
  discussion_id="$(json_get_discussion_id "$ARTIFACT_ROOT/native.discuss-open.json")"
  run_json "$repo" "native.discuss-append" discuss append "$discussion_id" "yes, this should round trip"

  run_json "$repo" "native.push.initial" push "$HEDDLE_SMOKE_REMOTE"
  run_json "$repo" "native.remote-list" remote list
  local origin_url
  origin_url="$(json_get_origin_url "$ARTIFACT_ROOT/native.remote-list.json")"

  printf '\nauto capture check\n' >> "$repo/README.md"
  echo "==> native.push.auto-capture: HEDDLE_AUTO_CAPTURE=command heddle push origin"
  (
    cd "$repo"
    HEDDLE_AUTO_CAPTURE=command heddle_runtime --output json push origin
  ) > "$ARTIFACT_ROOT/native.push.auto-capture.json"

  run_json "$repo" "native.pull" pull origin
  echo "==> native.clone: heddle clone $origin_url $clone"
  heddle_runtime --output json clone "$origin_url" "$clone" > "$ARTIFACT_ROOT/native.clone.json"
  run_json "$clone" "native.clone.verify" verify
  run_json "$clone" "native.clone.context-list" context list
  run_json "$clone" "native.clone.discuss-list" discuss list --status all
  json_assert_nonempty_items "$ARTIFACT_ROOT/native.clone.context-list.json" "native clone context list"
  json_assert_nonempty_items "$ARTIFACT_ROOT/native.clone.discuss-list.json" "native clone discussion list"
}

git_overlay_flow() {
  local repo="$WORK_ROOT/git-overlay-$RUN_ID"
  local clone="$WORK_ROOT/git-overlay-clone-$RUN_ID"
  mkdir -p "$repo/src"
  git -C "$repo" init --initial-branch=main >/dev/null
  configure_git_identity "$repo"
  printf '# Hosted Git Overlay Smoke\n\n' > "$repo/README.md"
  printf "pub fn run() -> &'static str { \"git overlay smoke\" }\n" > "$repo/src/lib.rs"
  git -C "$repo" add -A
  git -C "$repo" commit -m "seed git overlay smoke" >/dev/null

  (
    cd "$repo"
    heddle_runtime init \
      --principal-name "$HEDDLE_PRINCIPAL_NAME" \
      --principal-email "$HEDDLE_PRINCIPAL_EMAIL" >/dev/null
    printf '\ncheckpointed through heddle\n' >> README.md
    heddle_runtime capture -m "hosted git-overlay smoke capture" >/dev/null
    heddle_runtime commit -m "hosted git-overlay smoke commit" >/dev/null
  )

  run_json "$repo" "git-overlay.push" push "$HEDDLE_SMOKE_REMOTE"
  run_json "$repo" "git-overlay.remote-list" remote list
  local origin_url
  origin_url="$(json_get_origin_url "$ARTIFACT_ROOT/git-overlay.remote-list.json")"

  echo "==> git-overlay.clone: heddle clone $origin_url $clone"
  heddle_runtime --output json clone "$origin_url" "$clone" > "$ARTIFACT_ROOT/git-overlay.clone.json"
  run_json "$clone" "git-overlay.clone.verify" verify
}

native_flow
git_overlay_flow

echo "hosted release smoke passed"
echo "artifacts: $ARTIFACT_ROOT"
