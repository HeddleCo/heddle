#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HEDDLE_BIN="${HEDDLE_BIN:-$ROOT/target/debug/heddle}"

ARTIFACT_ROOT="${HEDDLE_TRUST_ARTIFACT_ROOT:-$ROOT/target/trust-cold-flow-agent}"
WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/heddle-trust-agent.XXXXXX")"
trap 'rm -rf "$WORK_ROOT"' EXIT
mkdir -p "$ARTIFACT_ROOT"
find "$ARTIFACT_ROOT" -mindepth 1 -maxdepth 1 -exec rm -rf {} +


configure_git() {
  local repo="$1"
  git -C "$repo" config user.name "Heddle Trust Flow"
  git -C "$repo" config user.email "trust-flow@example.com"
}

commit_all() {
  local repo="$1"
  local message="$2"
  git -C "$repo" add -A
  git -C "$repo" commit -m "$message" >/dev/null
}

create_fixture() {
  local repo="$1"
  local shape="$2"
  local origin="${repo}.origin.git"
  mkdir -p "$repo"
  git -C "$repo" init --initial-branch=main >/dev/null
  configure_git "$repo"

  case "$shape" in
    small-app)
      printf 'name = "small-app"\n' > "$repo/app.txt"
      commit_all "$repo" "seed small app"
      ;;
    large-rust)
      mkdir -p "$repo/src"
      cat > "$repo/Cargo.toml" <<'EOF'
[package]
name = "trust-large-rust"
version = "0.1.0"
edition = "2021"
EOF
      printf 'pub fn root() -> usize { 1 }\n' > "$repo/src/lib.rs"
      for n in $(seq 1 24); do
        mkdir -p "$repo/crates/member$n/src"
        printf 'pub fn member_%s() -> usize { %s }\n' "$n" "$n" > "$repo/crates/member$n/src/lib.rs"
      done
      commit_all "$repo" "seed large rust shape"
      ;;
    complex-git)
      mkdir -p "$repo/src" "$repo/assets"
      printf 'alpha\n' > "$repo/src/main.txt"
      printf '\001\002binary\003\004' > "$repo/assets/blob.bin"
      commit_all "$repo" "seed complex base"
      git -C "$repo" tag v1.0.0
      git -C "$repo" switch -c side >/dev/null
      printf 'side\n' > "$repo/src/side.txt"
      commit_all "$repo" "side branch work"
      git -C "$repo" switch main >/dev/null
      git -C "$repo" mv src/main.txt src/renamed.txt
      printf 'main\n' >> "$repo/src/renamed.txt"
      commit_all "$repo" "rename on main"
      git -C "$repo" merge --no-ff side -m "merge side" >/dev/null
      ;;
    *)
      echo "unknown shape: $shape" >&2
      exit 64
      ;;
  esac
  git clone --bare --quiet "$repo" "$origin"
}

assert_clean_git_status() {
  local repo="$1"
  local dirty
  dirty="$(git -C "$repo" status --short)"
  if [[ -n "$dirty" ]]; then
    echo "expected clean Git status in $repo, got:" >&2
    echo "$dirty" >&2
    exit 1
  fi
}

assert_final_trust() {
  local json_file="$1"
  python3 - "$json_file" <<'PYJSON'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
if data.get("trusted") is not True or data.get("status") != "clean":
    raise SystemExit(f"expected clean trusted report, got {data!r}")
PYJSON
}

assert_transcript_claims() {
  local transcript="$1"
  for needle in \
    '"bridge"' \
    '"checkpoint"' \
    '"commit"' \
    '"undo"' \
    '"fetch"' \
    '"pull"' \
    '"push"' \
    '"clone"' \
    '"reconcile"' \
    '"start"' \
    '"ready"' \
    '"merge"' \
    '"blame"' \
    '"git_checkpoint"' \
    '"semantic_result": "fast_forward"' \
    '"recommended_action"' \
    '"recovery_commands"' \
    '"Machine contract"' \
    '"trusted": true'; do
    if ! grep -F -- "$needle" "$transcript" >/dev/null; then
      echo "expected transcript to contain '$needle': $transcript" >&2
      exit 1
    fi
  done
  for forbidden in \
    "WARN " \
    "Failed to create marker" \
    '"recommended_action": "heddle merge main' \
    '"next_action": "heddle merge main'; do
    if grep -F -- "$forbidden" "$transcript" >/dev/null; then
      echo "transcript contains stale or raw internal output '$forbidden': $transcript" >&2
      exit 1
    fi
  done
}

run_json() {
  local transcript="$1"
  local repo="$2"
  local label="$3"
  shift 3
  local out="$ARTIFACT_ROOT/$label.json"
  printf '\n{"command": [' >> "$transcript"
  local first=1
  for arg in "$@"; do
    if [[ $first -eq 0 ]]; then printf ', ' >> "$transcript"; fi
    python3 -c 'import json,sys; print(json.dumps(sys.argv[1]), end="")' "$arg" >> "$transcript"
    first=0
  done
  printf ']}\n' >> "$transcript"
  (cd "$repo" && "$HEDDLE_BIN" "$@" --output json) > "$out"
  python3 -m json.tool "$out" >> "$transcript"
  python3 - "$out" <<'PYJSON'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    json.load(handle)
PYJSON
}

run_shape() {
  local shape="$1"
  local repo="$WORK_ROOT/$shape"
  local origin="${repo}.origin.git"
  local clone_path="$WORK_ROOT/$shape-clone"
  local transcript="$ARTIFACT_ROOT/$shape.jsonl"
  local final_json="$ARTIFACT_ROOT/$shape.final-trust.json"
  local clone_json="$ARTIFACT_ROOT/$shape.clone-trust.json"
  create_fixture "$repo" "$shape"
  : > "$transcript"

  run_json "$transcript" "$WORK_ROOT" "$shape.00.clone" clone "$origin" "$clone_path"
  run_json "$transcript" "$clone_path" "$shape.00.clone-trust" trust
  (cd "$clone_path" && "$HEDDLE_BIN" trust --output json) > "$clone_json"
  assert_final_trust "$clone_json"

  run_json "$transcript" "$repo" "$shape.01.status" status
  run_json "$transcript" "$repo" "$shape.02.trust-plain" trust
  test ! -e "$repo/.heddle"

  run_json "$transcript" "$repo" "$shape.03.init" init
  assert_clean_git_status "$repo"
  run_json "$transcript" "$repo" "$shape.04.trust-needs-import" trust
  run_json "$transcript" "$repo" "$shape.05.import" bridge git import
  run_json "$transcript" "$repo" "$shape.06.status-clean" status
  run_json "$transcript" "$repo" "$shape.07.doctor" doctor
  run_json "$transcript" "$repo" "$shape.08.bridge-status" bridge git status
  run_json "$transcript" "$repo" "$shape.09.reconcile-preview" bridge git reconcile --prefer heddle --ref main --preview
  run_json "$transcript" "$repo" "$shape.10.thread-list" thread list
  run_json "$transcript" "$repo" "$shape.11.thread-show" thread show
  run_json "$transcript" "$repo" "$shape.12.workspace-show" workspace show

  printf 'captured agent edit for %s\n' "$shape" >> "$repo/captured-agent-flow.txt"
  run_json "$transcript" "$repo" "$shape.13.diff-capture" diff
  run_json "$transcript" "$repo" "$shape.14.capture" capture -m "agent capture $shape"
  run_json "$transcript" "$repo" "$shape.15.checkpoint" checkpoint -m "agent checkpoint $shape"
  run_json "$transcript" "$repo" "$shape.16.push-checkpoint" bridge git push "$origin"
  run_json "$transcript" "$repo" "$shape.17.fetch" fetch "$origin"
  run_json "$transcript" "$repo" "$shape.18.pull" bridge git pull "$origin"
  assert_clean_git_status "$repo"

  printf 'agent edit for %s\n' "$shape" >> "$repo/agent-flow.txt"
  run_json "$transcript" "$repo" "$shape.19.diff-commit" diff
  run_json "$transcript" "$repo" "$shape.20.commit" commit -m "agent trust cold flow $shape"
  run_json "$transcript" "$repo" "$shape.21.undo" undo
  assert_clean_git_status "$repo"
  printf 'agent edit after undo for %s\n' "$shape" >> "$repo/agent-flow.txt"
  run_json "$transcript" "$repo" "$shape.22.commit-after-undo" commit -m "agent trust cold flow after undo $shape"
  run_json "$transcript" "$repo" "$shape.23.push-commit" bridge git push "$origin"
  run_json "$transcript" "$repo" "$shape.24.ready" ready
  assert_clean_git_status "$repo"
  run_json "$transcript" "$repo" "$shape.25.blame" blame agent-flow.txt
  run_json "$transcript" "$repo" "$shape.26.log" log

  local feature_thread="feature-$shape"
  local feature_path="$WORK_ROOT/$shape-isolated"
  run_json "$transcript" "$repo" "$shape.27.start" start "$feature_thread" --path "$feature_path" --workspace solid
  printf 'isolated agent work for %s\n' "$shape" > "$feature_path/isolated.txt"
  run_json "$transcript" "$feature_path" "$shape.28.diff-isolated" diff
  run_json "$transcript" "$feature_path" "$shape.29.capture-isolated" capture -m "isolated agent capture $shape"
  run_json "$transcript" "$feature_path" "$shape.30.ready-isolated" ready
  run_json "$transcript" "$repo" "$shape.31.merge-preview" merge "$feature_thread" --preview --with-diff
  run_json "$transcript" "$repo" "$shape.32.thread-show-feature" thread show "$feature_thread"

  (cd "$repo" && "$HEDDLE_BIN" trust --output json) > "$final_json"
  python3 -m json.tool "$final_json" >> "$transcript"
  assert_final_trust "$final_json"
  assert_transcript_claims "$transcript"
}

for shape in small-app large-rust complex-git; do
  run_shape "$shape"
done

echo "trust cold-flow agent transcripts: $ARTIFACT_ROOT"
