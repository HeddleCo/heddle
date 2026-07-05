#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HEDDLE_BIN="${HEDDLE_BIN:-$ROOT/target/debug/heddle}"
HEDDLE_RUNTIME_PATH="${HEDDLE_RUNTIME_PATH-}"
export HEDDLE_AGENT_PROVIDER=
export HEDDLE_AGENT_MODEL=
export HEDDLE_PRINCIPAL_NAME="${HEDDLE_PRINCIPAL_NAME:-Heddle Human}"
export HEDDLE_PRINCIPAL_EMAIL="${HEDDLE_PRINCIPAL_EMAIL:-human@example.com}"

ARTIFACT_ROOT="${HEDDLE_VERIFY_ARTIFACT_ROOT:-$ROOT/target/verify-cold-flow-human}"
WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/heddle-verify-human.XXXXXX")"
trap 'rm -rf "$WORK_ROOT"' EXIT
mkdir -p "$ARTIFACT_ROOT"
find "$ARTIFACT_ROOT" -mindepth 1 -maxdepth 1 -exec rm -rf {} +


heddle_runtime() {
  env PATH="$HEDDLE_RUNTIME_PATH" "$HEDDLE_BIN" "$@"
}

heddle_runtime_path_label() {
  if [[ -z "$HEDDLE_RUNTIME_PATH" ]]; then
    printf '<empty>'
  else
    printf '%s' "$HEDDLE_RUNTIME_PATH"
  fi
}

configure_git() {
  local repo="$1"
  git -C "$repo" config user.name "Heddle Verify Flow"
  git -C "$repo" config user.email "verify-flow@example.com"
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
name = "verify-large-rust"
version = "0.1.0"
edition = "2021"

[workspace]
members = [
EOF
      for n in $(seq -w 1 24); do
        printf '  "crates/member%s",\n' "$n" >> "$repo/Cargo.toml"
      done
      printf ']\n' >> "$repo/Cargo.toml"
      printf 'pub fn root() -> usize { 1 }\n' > "$repo/src/lib.rs"
      commit_all "$repo" "seed large rust workspace root"
      for group in 1 2 3; do
        start=$(( (group - 1) * 8 + 1 ))
        end=$(( group * 8 ))
        for i in $(seq "$start" "$end"); do
          n="$(printf '%02d' "$i")"
          mkdir -p "$repo/crates/member$n/src"
          cat > "$repo/crates/member$n/Cargo.toml" <<EOF
[package]
name = "member$n"
version = "0.1.0"
edition = "2021"
EOF
          printf 'pub fn member_%s() -> usize { %s }\n' "$n" "$n" > "$repo/crates/member$n/src/lib.rs"
        done
        commit_all "$repo" "add rust workspace members $start-$end"
      done
      mkdir -p "$repo/tests" "$repo/examples"
      printf '# Verify Large Rust\n\nWorkspace fixture with 24 member crates.\n' > "$repo/README.md"
      printf 'fn workspace_smoke() { assert_eq!(verify_large_rust::root(), 1); }\n' > "$repo/tests/workspace_smoke.rs"
      printf 'fn main() { println!(\"large rust fixture\"); }\n' > "$repo/examples/demo.rs"
      commit_all "$repo" "add workspace tests and examples"
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

append_shape_profile_text() {
  local transcript="$1"
  local shape="$2"
  case "$shape" in
    large-rust)
      cat >> "$transcript" <<'EOF'

Fixture profile: large Rust workspace
  proof: 24 member crates, workspace manifest, tests, examples, and multi-commit history
  daily edit: root module, member crate, generated module, and integration test
EOF
      ;;
    complex-git)
      cat >> "$transcript" <<'EOF'

Fixture profile: complex Git history
  proof: source Git tag v1.0.0 exists; scoped tag adopt reports Tags ready: 1 and thread marker list includes v1.0.0
  proof: merge commit, prior Git rename commit src/main.txt -> src/renamed.txt, binary assets/blob.bin, and side branch side
  daily edit: delete/add move src/renamed.txt -> src/final-name.txt, binary update, and merged-file follow-up
EOF
      ;;
  esac
}

make_human_main_edit() {
  local repo="$1"
  local shape="$2"
  local round="$3"
  case "$shape" in
    small-app)
      printf 'human edit %s for %s\n' "$round" "$shape" >> "$repo/flow.txt"
      ;;
    large-rust)
      printf '\npub mod generated;\n' >> "$repo/src/lib.rs"
      cat > "$repo/src/generated.rs" <<EOF
pub fn generated_$round() -> usize {
    42
}
EOF
      printf '\npub fn member_07_plus_%s() -> usize { member_07() + 1 }\n' "$round" >> "$repo/crates/member07/src/lib.rs"
      mkdir -p "$repo/tests"
      cat > "$repo/tests/workspace_smoke.rs" <<EOF
#[test]
fn workspace_smoke_$round() {
    assert_eq!(verify_large_rust::root(), 1);
}
EOF
      ;;
    complex-git)
      if [[ -f "$repo/src/renamed.txt" ]]; then
        mv "$repo/src/renamed.txt" "$repo/src/final-name.txt"
      fi
      printf 'human complex %s\n' "$round" >> "$repo/src/final-name.txt"
      printf '\005\006human-binary-%s\007' "$round" >> "$repo/assets/blob.bin"
      printf 'side branch follow-up from %s\n' "$round" >> "$repo/src/side.txt"
      ;;
  esac
}

attribution_target_for_shape() {
  local shape="$1"
  case "$shape" in
    small-app) printf 'flow.txt\n' ;;
    large-rust) printf 'src/generated.rs\n' ;;
    complex-git) printf 'src/final-name.txt\n' ;;
  esac
}

make_human_isolated_edit() {
  local repo="$1"
  local shape="$2"
  case "$shape" in
    small-app)
      printf 'isolated human work for %s\n' "$shape" > "$repo/isolated.txt"
      ;;
    large-rust)
      mkdir -p "$repo/tests" "$repo/examples"
      printf '#[test]\nfn isolated_thread_checks_member_18() { assert_eq!(18, 18); }\n' > "$repo/tests/isolated_thread.rs"
      printf 'fn main() { println!("isolated large Rust demo"); }\n' > "$repo/examples/isolated_demo.rs"
      ;;
    complex-git)
      printf 'isolated complex thread updates merged side work\n' >> "$repo/src/side.txt"
      printf '\010isolated-binary\011' >> "$repo/assets/blob.bin"
      ;;
  esac
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

assert_no_literal_round_leak() {
  local path="$1"
  if grep -IRF -- '$round' "$path" >/dev/null; then
    echo "generated transcript/artifact leaked literal "'$round'": $path" >&2
    grep -IRnF -- '$round' "$path" >&2
    exit 1
  fi
}

assert_final_verify() {
  local json_file="$1"
  python3 - "$json_file" <<'PYJSON'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
if data.get("output_kind") == "verify" and isinstance(data.get("verification"), dict):
    data = data["verification"]
if data.get("verified") is not True or data.get("status") != "clean":
    raise SystemExit(f"expected clean verified report, got {data!r}")
PYJSON
}

assert_first_run_init_ref() {
  local json_file="$1"
  local branch="$2"
  python3 - "$json_file" "$branch" <<'PYJSON'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
if data.get("kind") == "verify_failed":
    verification = data.get("verification")
    if not isinstance(verification, dict):
        raise SystemExit(f"verify_failed should include nested verification, got {data!r}")
    data = verification
elif data.get("output_kind") == "verify" and isinstance(data.get("verification"), dict):
    data = data["verification"]
branch = sys.argv[2]
argv = (data.get("recommended_action_template") or {}).get("argv_template") or []
heddle_argv = bool(argv) and (argv[0] == "heddle" or argv[0].endswith("/heddle"))
if data.get("verified") is not False or data.get("status") != "needs_init":
    raise SystemExit(f"first-run verify should require initialization, got {data!r}")
if data.get("recommended_action") != "heddle init" or not heddle_argv or argv[1:] != ["init"]:
    raise SystemExit(f"first-run verify should recommend initialization before adopting {branch}, got {data!r}")
checks = {check.get("name"): check for check in data.get("checks", [])}
if (checks.get("Mapping") or {}).get("status") != "git_backed":
    raise SystemExit(f"first-run verify should report direct Git-backed mapping, got {data!r}")
PYJSON
}

assert_first_run_init_all() {
  local json_file="$1"
  python3 - "$json_file" <<'PYJSON'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
if data.get("kind") == "verify_failed":
    verification = data.get("verification")
    if not isinstance(verification, dict):
        raise SystemExit(f"verify_failed should include nested verification, got {data!r}")
    data = verification
elif data.get("output_kind") == "verify" and isinstance(data.get("verification"), dict):
    data = data["verification"]
argv = (data.get("recommended_action_template") or {}).get("argv_template") or []
heddle_argv = bool(argv) and (argv[0] == "heddle" or argv[0].endswith("/heddle"))
if data.get("verified") is not False or data.get("status") != "needs_init":
    raise SystemExit(f"first-run verify should require initialization, got {data!r}")
if data.get("recommended_action") != "heddle init" or not heddle_argv or argv[1:] != ["init"]:
    raise SystemExit(f"first-run verify should recommend initialization, got {data!r}")
checks = {check.get("name"): check for check in data.get("checks", [])}
if (checks.get("Mapping") or {}).get("status") != "git_backed":
    raise SystemExit(f"first-run verify should report direct Git-backed mapping, got {data!r}")
PYJSON
}

capture_verify_failed_verification() {
  local repo="$1"
  local json_file="$2"
  local err_file="$3"
  local exit_code
  set +e
  (cd "$repo" && heddle_runtime verify --output json) > "$json_file.stdout" 2> "$err_file"
  exit_code=$?
  set -e
  python3 - "$json_file" "$json_file.stdout" "$err_file" "$exit_code" <<'PYJSON'
import json
import sys

json_path, stdout_path, stderr_path, exit_code_text = sys.argv[1:]
with open(stdout_path, encoding="utf-8") as handle:
    stdout = handle.read()
with open(stderr_path, encoding="utf-8") as handle:
    stderr = handle.read()
if int(exit_code_text) == 0:
    raise SystemExit("blocked verify unexpectedly exited 0")
if stdout:
    raise SystemExit(f"blocked JSON verify should not write stdout, got {stdout!r}")
stderr_lines = [line for line in stderr.splitlines() if line.strip()]
if len(stderr_lines) != 1:
    raise SystemExit(f"expected one stderr envelope, got {len(stderr_lines)} line(s): {stderr!r}")
envelope = json.loads(stderr)
if envelope.get("kind") != "verify_failed":
    raise SystemExit(f"expected verify_failed envelope, got {envelope!r}")
verification = envelope.get("verification")
if not isinstance(verification, dict):
    raise SystemExit(f"verify_failed envelope should include nested verification, got {envelope!r}")
if verification.get("verified") is not False:
    raise SystemExit(f"nested verification should be blocked, got {verification!r}")
with open(json_path, "w", encoding="utf-8") as handle:
    json.dump(verification, handle, sort_keys=True)
    handle.write("\n")
PYJSON
  rm -f "$json_file.stdout"
}

assert_ready_workflow() {
  local json_file="$1"
  local thread="$2"
  python3 - "$json_file" "$thread" <<'PYJSON'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
thread = sys.argv[2]
expected = f"heddle land --thread {thread} --no-push"
context_expected_suffix = ["land", "--thread", thread, "--no-push"]
verify = data.get("verification", data)
if verify.get("verified") is not True or verify.get("status") != "clean":
    raise SystemExit(f"ready work should keep repository verify clean, got {data!r}")
argv = (verify.get("recommended_action_template") or {}).get("argv_template") or []
context_ready = (
    len(argv) >= 6
    and (argv[0] == "heddle" or argv[0].endswith("/heddle"))
    and argv[1] == "--repo"
    and argv[-4:] == context_expected_suffix
)
plain_ready = (
    verify.get("recommended_action") == expected
    and len(argv) == 5
    and (argv[0] == "heddle" or argv[0].endswith("/heddle"))
    and argv[1:] == context_expected_suffix
)
if verify.get("workflow_status") != "ready" or not (context_ready or plain_ready):
    raise SystemExit(f"ready work should be represented as workflow_status=ready, got {data!r}")
PYJSON
}

assert_merge_preview_points_to_land() {
  local json_file="$1"
  local thread="$2"
  python3 - "$json_file" "$thread" <<'PYJSON'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
thread = sys.argv[2]
expected = f"heddle land --thread {thread} --no-push"
if data.get("preview_only") is not True or data.get("merge_relation") != "fast_forward":
    raise SystemExit(f"expected a clean fast-forward preview: {data!r}")
if data.get("recommended_action") != expected or data.get("next_action") != expected:
    raise SystemExit(f"merge preview should point to land, got {data!r}")
verify = data.get("verification")
if verify is not None:
    if verify.get("verified") is not True or verify.get("status") != "clean":
        raise SystemExit(f"merge preview should keep repository verify clean, got {data!r}")
    if verify.get("workflow_status") != "ready" or verify.get("recommended_action") != expected:
        raise SystemExit(f"nested verify should align with the preview's land action, got {data!r}")
PYJSON
}

assert_transcript_claims() {
  local transcript="$1"
  for needle in \
    "adopt" \
    "commit" \
    "undo" \
    "fetch" \
    "pull" \
    "push" \
    "clone" \
    "start" \
    "ready" \
    "--preview" \
    "Git repo detected" \
    "initialize Heddle with heddle init" \
    ".heddle metadata" \
    "Git worktree stays clean" \
    "query --attribution" \
    "saved: local Git commit recorded" \
    "merge type:" \
    "landed: on parent" \
    "push: not pushed" \
    "Next: heddle --repo" \
    "merge feature-" \
    "Next step: heddle land --thread feature-" \
    "--no-push" \
    "hd-" \
    "Workspace: verified" \
    "Nothing to do. Workspace verified."; do
    if ! grep -F -- "$needle" "$transcript" >/dev/null; then
      echo "expected transcript to contain '$needle': $transcript" >&2
      exit 1
    fi
  done
  for forbidden in \
    "WARN " \
    "[warn] Thread 'feature" \
    "Failed to create marker" \
    "next: heddle merge main" \
    "RUN: heddle capture -m" \
    "RUN: heddle checkpoint -m" \
    "Then: heddle land --thread" \
    "Nothing to capture" \
    "Last 5 captures" \
    "Captures on" \
    "materialized" \
    "heddle bridge git" \
    "reconcile" \
    "performed:" \
    "skipped:" \
    "already:" \
    "saved(no changes)" \
    "refreshed(current)" \
    "recommended action: heddle merge main" \
    "Next step: heddle ready --thread" \
    "Git-overlay refs" \
    "Agent: /" \
    "semantic: fast_forward" \
    "Workspace: solid" \
    "Threads in flight:" \
    "recommended action:" \
    "Machine contract   available" \
    "schemas partial" \
    "missing schemas" \
    "Machine contract   partial" \
    "Git overlay and Heddle agree" \
    "Git and Heddle:" \
    "initialized Heddle sidecar" \
    "Repository: plain-git" \
    "Git overlay:" \
    "Git checkpoint:" \
    "State: hd-" \
    "Intent:" \
    "Principal:" \
    "operation log" \
    "Mapping:"; do
    if grep -F -- "$forbidden" "$transcript" >/dev/null; then
      echo "transcript contains stale or raw internal output '$forbidden': $transcript" >&2
      exit 1
    fi
  done
}

assert_shape_transcript_claims() {
  local transcript="$1"
  local shape="$2"
  local needles=()
  case "$shape" in
    large-rust)
      needles=(
        "Fixture profile: large Rust workspace"
        "5 commits imported"
        "src/generated.rs"
        "crates/member07/src/lib.rs"
        "tests/workspace_smoke.rs"
        "tests/isolated_thread.rs"
        "examples/isolated_demo.rs"
      )
      ;;
    complex-git)
      needles=(
        "Fixture profile: complex Git history"
        "source Git tag v1.0.0 exists"
        "Tags ready: 1"
        "thread marker list includes v1.0.0"
        "v1.0.0 -> hd-"
        "delete/add move src/renamed.txt -> src/final-name.txt"
        "Branches ready: 2"
        "side clean imported Git branch"
        "merge side"
        "rename on main"
        "src/final-name.txt"
        "assets/blob.bin"
        "src/side.txt"
      )
      ;;
    *)
      return 0
      ;;
  esac
  for needle in "${needles[@]}"; do
    if ! grep -F -- "$needle" "$transcript" >/dev/null; then
      echo "expected $shape transcript to prove '$needle': $transcript" >&2
      exit 1
    fi
  done
}

run_text() {
  local transcript="$1"
  local repo="$2"
  local allow_failure="${RUN_TEXT_ALLOW_FAILURE:-0}"
  shift 2
  local args=("$@")
  printf '\n$ (cd %s && heddle' "$repo" >> "$transcript"
  for arg in "${args[@]}"; do
    printf ' %q' "$arg" >> "$transcript"
  done
  printf ')\n' >> "$transcript"
  local exit_code
  set +e
  (cd "$repo" && heddle_runtime "${args[@]}") >> "$transcript" 2>&1
  exit_code=$?
  set -e
  if [[ "$exit_code" -ne 0 && "$allow_failure" != "1" ]]; then
    return "$exit_code"
  fi
}

run_text_expect_failure() {
  RUN_TEXT_ALLOW_FAILURE=1 run_text "$@"
}

assert_current_verify_clean() {
  local repo="$1"
  local json
  json="$(cd "$repo" && heddle_runtime verify --output json)"
  python3 - "$json" <<'PYJSON'
import json
import sys
data = json.loads(sys.argv[1])
if data.get("output_kind") == "verify" and isinstance(data.get("verification"), dict):
    data = data["verification"]
if data.get("verified") is not True or data.get("status") != "clean":
    raise SystemExit(f"expected recommended action to restore clean verify, got {data!r}")
PYJSON
}

run_verify_recommended_action_text() {
  local transcript="$1"
  local repo="$2"
  local message="${3:-human verify cold flow}"
  mapfile -t action_args < <(
    (cd "$repo" && heddle_runtime status --output json) | python3 -c '
import json
import sys
data = json.load(sys.stdin)
data = data.get("verification", data)
template = data.get("recommended_action_template")
if not template:
    raise SystemExit(f"expected template recommended action: {data!r}")
if template.get("action") != data.get("recommended_action"):
    raise SystemExit(f"template/action mismatch: {data!r}")
if template.get("required_inputs") != ["message"]:
    raise SystemExit(f"unexpected template inputs: {template!r}")
message = sys.argv[1]
args = [
    message if arg == "<message>" else arg
    for arg in template.get("argv_template", [])
]
if not args or (args[0] != "heddle" and not args[0].endswith("/heddle")):
    raise SystemExit(f"unexpected recommended action template: {template!r}")
for arg in args[1:]:
    print(arg)
' "$message"
  )
  run_text "$transcript" "$repo" "${action_args[@]}" --output text
  assert_current_verify_clean "$repo"
}

run_shape() {
  local shape="$1"
  local repo="$WORK_ROOT/$shape"
  local origin="${repo}.origin.git"
  local clone_path="$WORK_ROOT/$shape-clone"
  local transcript="$ARTIFACT_ROOT/$shape.txt"
  local final_json="$ARTIFACT_ROOT/$shape.final-verify.json"
  local clone_json="$ARTIFACT_ROOT/$shape.clone-verify.json"
  local plain_verify_json="$ARTIFACT_ROOT/$shape.plain-verify.json"
  local ready_verify_json="$ARTIFACT_ROOT/$shape.ready-verify.json"
  local merge_preview_json="$ARTIFACT_ROOT/$shape.merge-preview.json"
  local plain_verify_err="$ARTIFACT_ROOT/$shape.plain-verify.stderr"
  create_fixture "$repo" "$shape"
  : > "$transcript"
  printf 'Heddle runtime proof: every heddle command in this transcript ran with PATH=%s; Git was used only to build fixture repositories before the cold story began.\n' "$(heddle_runtime_path_label)" >> "$transcript"
  append_shape_profile_text "$transcript" "$shape"

  run_text "$transcript" "$WORK_ROOT" clone "$origin" "$clone_path" --output text
  run_text "$transcript" "$clone_path" verify --output text
  (cd "$clone_path" && heddle_runtime verify --output json) > "$clone_json"
  assert_final_verify "$clone_json"

  run_text "$transcript" "$repo" status --output text
  capture_verify_failed_verification "$repo" "$plain_verify_json" "$plain_verify_err"
  if [[ "$shape" == "complex-git" ]]; then
    assert_first_run_init_all "$plain_verify_json"
  else
    assert_first_run_init_ref "$plain_verify_json" main
  fi
  test ! -e "$repo/.heddle"

  if [[ "$shape" == "complex-git" ]]; then
    run_text "$transcript" "$repo" adopt --output text
  else
    run_text "$transcript" "$repo" adopt --ref main --output text
  fi
  assert_clean_git_status "$repo"
  if [[ "$shape" == "complex-git" ]]; then
    run_text "$transcript" "$repo" adopt --ref v1.0.0 --output text
    run_text "$transcript" "$repo" thread marker list --output text
  fi
  run_text "$transcript" "$repo" verify --output text
  run_text "$transcript" "$repo" status --output text
  run_text "$transcript" "$repo" doctor --output text
  run_text "$transcript" "$repo" thread list --output text
  run_text "$transcript" "$repo" thread show --output text
  run_text "$transcript" "$repo" status --output text

  make_human_main_edit "$repo" "$shape" first
  run_text "$transcript" "$repo" diff --output text
  if [[ "$shape" != "small-app" ]]; then
    run_text "$transcript" "$repo" diff --stat --output text
    run_text "$transcript" "$repo" diff --name-only --output text
  fi
  run_text "$transcript" "$repo" status --output text
  capture_verify_failed_verification "$repo" "$ARTIFACT_ROOT/$shape.dirty-verify.json" "$ARTIFACT_ROOT/$shape.dirty-verify.stderr"
  run_verify_recommended_action_text "$transcript" "$repo" "verify cold flow $shape"
  run_text "$transcript" "$repo" undo --output text
  assert_clean_git_status "$repo"
  make_human_main_edit "$repo" "$shape" after_undo
  run_text "$transcript" "$repo" status --output text
  capture_verify_failed_verification "$repo" "$ARTIFACT_ROOT/$shape.after-undo-dirty-verify.json" "$ARTIFACT_ROOT/$shape.after-undo-dirty-verify.stderr"
  run_verify_recommended_action_text "$transcript" "$repo" "verify cold flow after undo $shape"
  run_text "$transcript" "$repo" push "$origin" --output text
  run_text "$transcript" "$repo" fetch "$origin" --output text
  run_text "$transcript" "$repo" pull "$origin" --output text
  run_text "$transcript" "$repo" ready --output text
  assert_clean_git_status "$repo"
  run_text "$transcript" "$repo" query --attribution "$(attribution_target_for_shape "$shape")" --output text
  run_text "$transcript" "$repo" log --output text

  local feature_thread="feature-$shape"
  local feature_path="$WORK_ROOT/$shape-isolated"
  run_text "$transcript" "$repo" start "$feature_thread" --path "$feature_path" --workspace solid --output text
  make_human_isolated_edit "$feature_path" "$shape"
  run_text "$transcript" "$feature_path" diff --output text
  if [[ "$shape" != "small-app" ]]; then
    run_text "$transcript" "$feature_path" diff --stat --output text
    run_text "$transcript" "$feature_path" diff --name-only --output text
  fi
  run_text_expect_failure "$transcript" "$feature_path" ready --output text
  run_text "$transcript" "$feature_path" ready -m "isolated human ready $shape" --output text
  (cd "$feature_path" && heddle_runtime ready --output json) > "$ready_verify_json"
  assert_ready_workflow "$ready_verify_json" "$feature_thread"
  run_text "$transcript" "$repo" merge "$feature_thread" --preview --with-diff --output text
  (cd "$repo" && heddle_runtime merge "$feature_thread" --preview --with-diff --output json) > "$merge_preview_json"
  assert_merge_preview_points_to_land "$merge_preview_json" "$feature_thread"
  run_text "$transcript" "$repo" thread show "$feature_thread" --output text
  run_text "$transcript" "$repo" land --thread "$feature_thread" --no-push --output text
  run_text "$transcript" "$repo" verify --output text
  run_text "$transcript" "$repo" push "$origin" --output text

  run_text "$transcript" "$repo" verify --output text
  (cd "$repo" && heddle_runtime verify --output json) > "$final_json"
  assert_final_verify "$final_json"
  assert_transcript_claims "$transcript"
  assert_shape_transcript_claims "$transcript" "$shape"
  assert_no_literal_round_leak "$ARTIFACT_ROOT"
}

for shape in small-app large-rust complex-git; do
  run_shape "$shape"
done

assert_no_literal_round_leak "$ARTIFACT_ROOT"

echo "verify cold-flow human transcripts: $ARTIFACT_ROOT"
