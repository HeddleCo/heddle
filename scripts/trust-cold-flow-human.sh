#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HEDDLE_BIN="${HEDDLE_BIN:-$ROOT/target/debug/heddle}"

ARTIFACT_ROOT="${HEDDLE_TRUST_ARTIFACT_ROOT:-$ROOT/target/trust-cold-flow-human}"
WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/heddle-trust-human.XXXXXX")"
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
    "bridge git import" \
    "checkpoint" \
    "commit" \
    "undo" \
    "fetch" \
    "pull" \
    "push" \
    "clone" \
    "reconcile" \
    "start" \
    "ready" \
    "--preview" \
    "blame" \
    "Git checkpoint:" \
    "semantic preview:" \
    "hd-" \
    '"trusted":true'; do
    if ! grep -F -- "$needle" "$transcript" >/dev/null; then
      echo "expected transcript to contain '$needle': $transcript" >&2
      exit 1
    fi
  done
  for forbidden in \
    "WARN " \
    "Failed to create marker" \
    "next: heddle merge main" \
    "recommended action: heddle merge main"; do
    if grep -F -- "$forbidden" "$transcript" >/dev/null; then
      echo "transcript contains stale or raw internal output '$forbidden': $transcript" >&2
      exit 1
    fi
  done
}

run_text() {
  local transcript="$1"
  local repo="$2"
  shift 2
  local args=("$@")
  printf '\n$ (cd %s && %q' "$repo" "${args[0]}" >> "$transcript"
  for arg in "${args[@]:1}"; do
    printf ' %q' "$arg" >> "$transcript"
  done
  printf ')\n' >> "$transcript"
  (cd "$repo" && "$HEDDLE_BIN" "${args[@]}") >> "$transcript" 2>&1
}

run_shape() {
  local shape="$1"
  local repo="$WORK_ROOT/$shape"
  local origin="${repo}.origin.git"
  local clone_path="$WORK_ROOT/$shape-clone"
  local transcript="$ARTIFACT_ROOT/$shape.txt"
  local final_json="$ARTIFACT_ROOT/$shape.final-trust.json"
  local clone_json="$ARTIFACT_ROOT/$shape.clone-trust.json"
  create_fixture "$repo" "$shape"
  : > "$transcript"

  run_text "$transcript" "$WORK_ROOT" clone "$origin" "$clone_path" --output text
  run_text "$transcript" "$clone_path" trust --output text
  (cd "$clone_path" && "$HEDDLE_BIN" trust --output json) > "$clone_json"
  assert_final_trust "$clone_json"

  run_text "$transcript" "$repo" status --output text
  run_text "$transcript" "$repo" trust --output text
  test ! -e "$repo/.heddle"

  run_text "$transcript" "$repo" init --output text
  assert_clean_git_status "$repo"
  run_text "$transcript" "$repo" trust --output text
  run_text "$transcript" "$repo" bridge git import --output text
  run_text "$transcript" "$repo" status --output text
  run_text "$transcript" "$repo" doctor --output text
  run_text "$transcript" "$repo" bridge git status --output text
  run_text "$transcript" "$repo" bridge git reconcile --prefer heddle --ref main --preview --output text
  run_text "$transcript" "$repo" thread list --output text
  run_text "$transcript" "$repo" thread show --output text
  run_text "$transcript" "$repo" workspace show --output text

  printf 'captured human edit for %s\n' "$shape" >> "$repo/captured-flow.txt"
  run_text "$transcript" "$repo" diff --output text
  run_text "$transcript" "$repo" capture -m "human capture $shape" --output text
  run_text "$transcript" "$repo" checkpoint -m "human checkpoint $shape" --output text
  run_text "$transcript" "$repo" bridge git push "$origin" --output text
  run_text "$transcript" "$repo" fetch "$origin" --output text
  run_text "$transcript" "$repo" bridge git pull "$origin" --output text
  assert_clean_git_status "$repo"

  printf 'human edit for %s\n' "$shape" >> "$repo/flow.txt"
  run_text "$transcript" "$repo" diff --output text
  run_text "$transcript" "$repo" commit -m "trust cold flow $shape" --output text
  run_text "$transcript" "$repo" undo --output text
  assert_clean_git_status "$repo"
  printf 'human edit after undo for %s\n' "$shape" >> "$repo/flow.txt"
  run_text "$transcript" "$repo" commit -m "trust cold flow after undo $shape" --output text
  run_text "$transcript" "$repo" bridge git push "$origin" --output text
  run_text "$transcript" "$repo" ready --output text
  assert_clean_git_status "$repo"
  run_text "$transcript" "$repo" blame flow.txt --output text
  run_text "$transcript" "$repo" log --output text

  local feature_thread="feature-$shape"
  local feature_path="$WORK_ROOT/$shape-isolated"
  run_text "$transcript" "$repo" start "$feature_thread" --path "$feature_path" --workspace solid --output text
  printf 'isolated human work for %s\n' "$shape" > "$feature_path/isolated.txt"
  run_text "$transcript" "$feature_path" diff --output text
  run_text "$transcript" "$feature_path" capture -m "isolated human capture $shape" --output text
  run_text "$transcript" "$feature_path" ready --output text
  run_text "$transcript" "$repo" merge "$feature_thread" --preview --with-diff --output text
  run_text "$transcript" "$repo" thread show "$feature_thread" --output text

  (cd "$repo" && "$HEDDLE_BIN" trust --output json) > "$final_json"
  cat "$final_json" >> "$transcript"
  assert_final_trust "$final_json"
  assert_transcript_claims "$transcript"
}

for shape in small-app large-rust complex-git; do
  run_shape "$shape"
done

echo "trust cold-flow human transcripts: $ARTIFACT_ROOT"
