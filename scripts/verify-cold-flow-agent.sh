#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HEDDLE_BIN="${HEDDLE_BIN:-$ROOT/target/debug/heddle}"
HEDDLE_RUNTIME_PATH="${HEDDLE_RUNTIME_PATH-}"
export HEDDLE_AGENT_PROVIDER="${HEDDLE_AGENT_PROVIDER:-codex-cli}"
export HEDDLE_AGENT_MODEL="${HEDDLE_AGENT_MODEL:-oss-cold-flow}"
export HEDDLE_PRINCIPAL_NAME="${HEDDLE_PRINCIPAL_NAME:-Codex Evaluation}"
export HEDDLE_PRINCIPAL_EMAIL="${HEDDLE_PRINCIPAL_EMAIL:-codex-eval@example.com}"

ARTIFACT_ROOT="${HEDDLE_VERIFY_ARTIFACT_ROOT:-$ROOT/target/verify-cold-flow-agent}"
WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/heddle-verify-agent.XXXXXX")"
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

append_shape_profile_json() {
  local transcript="$1"
  local shape="$2"
  python3 - "$transcript" "$shape" <<'PYJSON'
import json
import sys
transcript, shape = sys.argv[1:]
profiles = {
    "large-rust": {
        "repo_shape": "large-rust",
        "proof": [
            "24 member crates",
            "workspace manifest",
            "tests",
            "examples",
            "multi-commit history",
        ],
        "daily_edit_paths": [
            "src/generated.rs",
            "crates/member07/src/lib.rs",
            "tests/workspace_smoke.rs",
            "tests/isolated_thread.rs",
            "examples/isolated_demo.rs",
        ],
    },
    "complex-git": {
        "repo_shape": "complex-git",
        "proof": [
            "source Git tag v1.0.0 exists before import; scoped tag adopt reports tags_synced=1 and marker show resolves v1.0.0",
            "merge commit",
            "prior Git rename commit src/main.txt -> src/renamed.txt",
            "binary assets/blob.bin",
            "side branch side",
        ],
        "daily_edit_paths": [
            "src/final-name.txt",
            "assets/blob.bin",
            "src/side.txt",
        ],
        "daily_edit_shape": "delete/add move src/renamed.txt -> src/final-name.txt plus binary and merged-file follow-up",
    },
}
profile = profiles.get(shape)
if profile:
    record = {
        "command": ["fixture-profile", shape],
        "output": profile,
        "exit_code": 0,
        "stdout": json.dumps(profile, sort_keys=True),
        "stderr": "",
    }
    with open(transcript, "a", encoding="utf-8") as handle:
        print(json.dumps(record, sort_keys=True), file=handle)
PYJSON
}

make_agent_capture_edit() {
  local repo="$1"
  local shape="$2"
  case "$shape" in
    small-app)
      printf 'captured agent edit for %s\n' "$shape" >> "$repo/captured-agent-flow.txt"
      ;;
    large-rust)
      printf '\npub mod captured;\n' >> "$repo/src/lib.rs"
      printf 'pub fn captured_agent() -> usize { 86 }\n' > "$repo/src/captured.rs"
      printf '\npub fn captured_member_03() -> usize { member_03() + 3 }\n' >> "$repo/crates/member03/src/lib.rs"
      ;;
    complex-git)
      printf 'agent capture updates merged side work\n' >> "$repo/src/side.txt"
      printf '\012agent-capture-binary\013' >> "$repo/assets/blob.bin"
      ;;
  esac
}

make_agent_commit_edit() {
  local repo="$1"
  local shape="$2"
  local round="$3"
  case "$shape" in
    small-app)
      printf 'agent edit %s for %s\n' "$round" "$shape" >> "$repo/agent-flow.txt"
      ;;
    large-rust)
      printf '\npub mod generated;\n' >> "$repo/src/lib.rs"
      cat > "$repo/src/generated.rs" <<EOF
pub fn generated_$round() -> usize {
    90
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
      printf 'agent complex %s\n' "$round" >> "$repo/src/final-name.txt"
      printf '\014agent-binary-%s\015' "$round" >> "$repo/assets/blob.bin"
      printf 'agent side branch follow-up from %s\n' "$round" >> "$repo/src/side.txt"
      ;;
  esac
}

agent_blame_target_for_shape() {
  local shape="$1"
  case "$shape" in
    small-app) printf 'agent-flow.txt\n' ;;
    large-rust) printf 'src/generated.rs\n' ;;
    complex-git) printf 'src/final-name.txt\n' ;;
  esac
}

make_agent_isolated_edit() {
  local repo="$1"
  local shape="$2"
  case "$shape" in
    small-app)
      printf 'isolated agent work for %s\n' "$shape" > "$repo/isolated.txt"
      ;;
    large-rust)
      mkdir -p "$repo/tests" "$repo/examples"
      printf '#[test]\nfn isolated_thread_checks_member_18() { assert_eq!(18, 18); }\n' > "$repo/tests/isolated_thread.rs"
      printf 'fn main() { println!("isolated large Rust demo"); }\n' > "$repo/examples/isolated_demo.rs"
      ;;
    complex-git)
      printf 'isolated complex thread updates merged side work\n' >> "$repo/src/side.txt"
      printf '\016isolated-agent-binary\017' >> "$repo/assets/blob.bin"
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
if data.get("verified") is not True or data.get("status") != "clean":
    raise SystemExit(f"expected clean verified report, got {data!r}")
PYJSON
}

assert_first_run_adopt_ref_json() {
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
branch = sys.argv[2]
expected = f"heddle adopt --ref {branch}"
expected_suffix = ["adopt", "--ref", branch]
argv = (data.get("recommended_action_template") or {}).get("argv_template") or []
heddle_argv = bool(argv) and (argv[0] == "heddle" or argv[0].endswith("/heddle"))
if data.get("verified") is not False or data.get("status") != "needs_init":
    raise SystemExit(f"first-run verify should require adoption, got {data!r}")
if data.get("recommended_action") != expected or not heddle_argv or argv[1:] != expected_suffix:
    raise SystemExit(f"first-run verify should recommend scoped adoption, got {data!r}")
checks = {check.get("name"): check for check in data.get("checks", [])}
for name in ("Heddle", "Mapping"):
    check = checks.get(name) or {}
    check_argv = (check.get("recommended_action_template") or {}).get("argv_template") or []
    check_heddle_argv = bool(check_argv) and (
        check_argv[0] == "heddle" or check_argv[0].endswith("/heddle")
    )
    if check.get("recommended_action") != expected or not check_heddle_argv or check_argv[1:] != expected_suffix:
        raise SystemExit(f"{name} check should recommend scoped adoption, got {data!r}")
PYJSON
}

assert_first_run_adopt_all_json() {
  local json_file="$1"
  python3 - "$json_file" <<'PYJSON'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
if data.get("kind") == "verify_failed":
    data = data.get("verification") or {}
argv = (data.get("recommended_action_template") or {}).get("argv_template") or []
heddle_argv = bool(argv) and (argv[0] == "heddle" or argv[0].endswith("/heddle"))
if data.get("verified") is not False or data.get("status") != "needs_init":
    raise SystemExit(f"first-run verify should require adoption, got {data!r}")
if data.get("recommended_action") != "heddle adopt" or not heddle_argv or argv[1:] != ["adopt"]:
    raise SystemExit(f"first-run verify should recommend full adoption, got {data!r}")
checks = {check.get("name"): check for check in data.get("checks", [])}
for name in ("Heddle", "Mapping"):
    check = checks.get(name) or {}
    check_argv = (check.get("recommended_action_template") or {}).get("argv_template") or []
    check_heddle_argv = bool(check_argv) and (
        check_argv[0] == "heddle" or check_argv[0].endswith("/heddle")
    )
    if check.get("recommended_action") != "heddle adopt" or not check_heddle_argv or check_argv[1:] != ["adopt"]:
        raise SystemExit(f"{name} check should recommend full adoption, got {data!r}")
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
    '"agent": "codex-cli/oss-cold-flow"' \
    '"confidence": 0.' \
    '"recommended_action"' \
    '"remote_drift": "remote_ahead"' \
    '"clone_verification": "verified"' \
    '"recommended_action_template"' \
    '"recommended_action_templates"' \
    '"recovery_commands"' \
    '"side_effects"' \
    '"supports_op_id": true' \
    '"--op-id"' \
    '"Machine contract"' \
    '"machine_contract_coverage"' \
    '"json_commands_without_schema"' \
    '"supports_op_id_total"' \
    '"stdout"' \
    '"stderr"' \
    '"exit_code": 0' \
    '"exit_code": 1' \
    '"error_output"' \
    '"kind": "thread_not_found"' \
    '"hint"' \
    '"verified": true'; do
    if ! grep -F -- "$needle" "$transcript" >/dev/null; then
      echo "expected transcript to contain '$needle': $transcript" >&2
      exit 1
    fi
  done
  for forbidden in \
    "WARN " \
    "Failed to create marker" \
    '"recommended_action": "heddle merge main' \
    '"next_action": "heddle merge main' \
    '"agent": "/"' \
    "Git-overlay refs" \
    "Workspace: solid"; do
    if grep -F -- "$forbidden" "$transcript" >/dev/null; then
      echo "transcript contains stale or raw internal output '$forbidden': $transcript" >&2
      exit 1
    fi
  done
}

assert_shape_transcript_claims_json() {
  local transcript="$1"
  local shape="$2"
  local needles=()
  case "$shape" in
    large-rust)
      needles=(
        '"repo_shape": "large-rust"'
        '"24 member crates"'
        '"commits_imported": 5'
        'src/generated.rs'
        'crates/member07/src/lib.rs'
        'tests/workspace_smoke.rs'
        'tests/isolated_thread.rs'
        'examples/isolated_demo.rs'
      )
      ;;
    complex-git)
      needles=(
        '"repo_shape": "complex-git"'
        'source Git tag v1.0.0 exists before import'
        '"tags_synced": 1'
        'marker show resolves v1.0.0'
        '"name": "v1.0.0"'
        'delete/add move src/renamed.txt -> src/final-name.txt'
        '"side branch side"'
        '"available_git_refs"'
        'merge side'
        'rename on main'
        'src/final-name.txt'
        'assets/blob.bin'
        'src/side.txt'
      )
      ;;
    *)
      return 0
      ;;
  esac
  for needle in "${needles[@]}"; do
    if ! grep -F -- "$needle" "$transcript" >/dev/null; then
      echo "expected $shape JSONL transcript to prove '$needle': $transcript" >&2
      exit 1
    fi
  done
}

run_json() {
  local transcript="$1"
  local repo="$2"
  local label="$3"
  local allow_failure="${RUN_JSON_ALLOW_FAILURE:-0}"
  shift 3
  local out="$ARTIFACT_ROOT/$label.json"
  local err="$ARTIFACT_ROOT/$label.stderr"
  local exit_code
  set +e
  (cd "$repo" && heddle_runtime "$@" --output json) > "$out" 2> "$err"
  exit_code=$?
  set -e
  python3 - "$transcript" "$out" "$err" "$exit_code" "$@" <<'PYJSON'
import json
import sys
transcript, out_path, err_path, exit_code_text, *args = sys.argv[1:]
with open(out_path, encoding="utf-8") as handle:
    stdout = handle.read()
with open(err_path, encoding="utf-8") as handle:
    stderr = handle.read()
if int(exit_code_text) == 0 and stderr.strip():
    raise SystemExit(f"successful JSON command wrote stderr: {' '.join(args)}: {stderr!r}")
output = None
if stdout.strip():
    output = json.loads(stdout)
error = None
if stderr.strip():
    try:
        error = json.loads(stderr)
    except json.JSONDecodeError:
        error = None
if int(exit_code_text) != 0 and output is None and error is not None:
    output = error
    with open(out_path, "w", encoding="utf-8") as handle:
        json.dump(output, handle, sort_keys=True)
        handle.write("\n")
record = {
    "command": ["heddle", *args, "--output", "json"],
    "stdout": stdout,
    "output": output,
    "exit_code": int(exit_code_text),
    "stderr": stderr,
}
if error is not None:
    record["error_output"] = error
with open(sys.argv[1], "a", encoding="utf-8") as handle:
    print(json.dumps(record), file=handle)
PYJSON
  if [[ "$exit_code" -ne 0 && "$allow_failure" != "1" ]]; then
    echo "command failed for $label: heddle $* --output json" >&2
    cat "$err" >&2
    return "$exit_code"
  fi
}

run_json_expect_failure() {
  local transcript="$1"
  local repo="$2"
  local label="$3"
  shift 3
  RUN_JSON_ALLOW_FAILURE=1 run_json "$transcript" "$repo" "$label" "$@"
  python3 - "$ARTIFACT_ROOT/$label.json" "$ARTIFACT_ROOT/$label.stderr" "$transcript" <<'PYJSON'
import json
import sys
json_path, stderr_path, transcript_path = sys.argv[1:]
with open(json_path, encoding="utf-8") as handle:
    artifact = json.load(handle)
with open(stderr_path, encoding="utf-8") as handle:
    error = json.load(handle)
if artifact != error:
    raise SystemExit(f"failure json artifact should mirror stderr error envelope: artifact={artifact!r}, stderr={error!r}")
for key in ("kind", "error", "hint"):
    if not artifact.get(key):
        raise SystemExit(f"expected non-empty {key} in error envelope: {artifact!r}")
with open(transcript_path, encoding="utf-8") as handle:
    records = [json.loads(line) for line in handle if line.strip()]
record = records[-1]
if record.get("exit_code") == 0:
    raise SystemExit(f"expected failing command exit code, got {record!r}")
if not record.get("stderr"):
    raise SystemExit(f"expected failing command stderr to be recorded, got {record!r}")
if record.get("output") != artifact:
    raise SystemExit(f"expected transcript output to mirror failure artifact, got {record!r}")
if record.get("error_output") != artifact:
    raise SystemExit(f"expected transcript error_output to be retained, got {record!r}")
PYJSON
}

run_json_expect_verify_failed() {
  local transcript="$1"
  local repo="$2"
  local label="$3"
  shift 3
  run_json_expect_failure "$transcript" "$repo" "$label" "$@"
  python3 - "$ARTIFACT_ROOT/$label.json" "$ARTIFACT_ROOT/$label.stderr" <<'PYJSON'
import json
import sys

artifact_path, stderr_path = sys.argv[1:]
with open(artifact_path, encoding="utf-8") as handle:
    artifact = json.load(handle)
with open(stderr_path, encoding="utf-8") as handle:
    stderr = handle.read()
stderr_lines = [line for line in stderr.splitlines() if line.strip()]
if len(stderr_lines) != 1:
    raise SystemExit(f"expected one stderr envelope, got {len(stderr_lines)} line(s): {stderr!r}")
if artifact.get("kind") != "verify_failed":
    raise SystemExit(f"expected verify_failed envelope, got {artifact!r}")
verification = artifact.get("verification")
if not isinstance(verification, dict):
    raise SystemExit(f"verify_failed should include nested verification, got {artifact!r}")
if verification.get("verified") is not False:
    raise SystemExit(f"nested verification should be blocked, got {verification!r}")
PYJSON
}

assert_current_verify_clean_json() {
  local repo="$1"
  local json
  json="$(cd "$repo" && heddle_runtime verify --output json)"
  python3 - "$json" <<'PYJSON'
import json
import sys
data = json.loads(sys.argv[1])
if data.get("verified") is not True or data.get("status") != "clean":
    raise SystemExit(f"expected recommended action to restore clean verify, got {data!r}")
PYJSON
}

assert_current_worktree_clean_json() {
  local repo="$1"
  local json
  json="$(cd "$repo" && heddle_runtime verify --output json)"
  python3 - "$json" <<'PYJSON'
import json
import sys
data = json.loads(sys.argv[1])
checks = {
    check.get("name"): check
    for check in data.get("checks", [])
}
worktree = checks.get("Worktree")
if not worktree or worktree.get("status") != "clean" or worktree.get("clean") is not True:
    raise SystemExit(f"expected recommended action to clear the worktree blocker, got {data!r}")
PYJSON
}

assert_local_ahead_verified_json() {
  local json_file="$1"
  python3 - "$json_file" <<'PYJSON'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
verify = data.get("verification", data)
checks = {check.get("name"): check for check in verify.get("checks", [])}
remote = checks.get("Remote") or {}
if verify.get("verified") is not True or verify.get("status") != "clean":
    raise SystemExit(f"local-ahead work should remain verified and clean: {data!r}")
if verify.get("remote_drift") != "remote_ahead":
    raise SystemExit(f"expected remote_ahead sync state: {data!r}")
if verify.get("recommended_action") != "heddle push":
    raise SystemExit(f"expected push guidance for local-ahead work: {data!r}")
if verify.get("clone_verification") != "verified":
    raise SystemExit(f"local-ahead work should not block clone verification: {data!r}")
if remote.get("clean") is not True or remote.get("status") != "remote_ahead":
    raise SystemExit(f"remote_ahead should be a clean remote check: {data!r}")
if remote.get("recovery_commands"):
    raise SystemExit(f"remote_ahead should be guidance, not recovery: {data!r}")
PYJSON
}

assert_merge_preview_points_to_ship_json() {
  local json_file="$1"
  local thread="$2"
  python3 - "$json_file" "$thread" <<'PYJSON'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
thread = sys.argv[2]
expected = f"heddle ship --thread {thread} --no-push"
if data.get("preview_only") is not True or data.get("semantic_result") != "fast_forward":
    raise SystemExit(f"expected a clean fast-forward preview: {data!r}")
if data.get("recommended_action") != expected or data.get("next_action") != expected:
    raise SystemExit(f"merge preview should point to ship, got {data!r}")
verify = data.get("verification") or {}
if verify.get("verified") is not True or verify.get("status") != "clean":
    raise SystemExit(f"merge preview should keep repository verify clean while workflow remains actionable: {data!r}")
if verify.get("workflow_status") != "ready" or verify.get("recommended_action") != expected:
    raise SystemExit(f"nested verify should align with the preview's ship action: {data!r}")
PYJSON
}

assert_fetch_reports_verification_json() {
  local json_file="$1"
  python3 - "$json_file" <<'PYJSON'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
verify = data.get("verification") or {}
if verify.get("verified") is not True or not verify.get("status"):
    raise SystemExit(f"fetch should carry post-command verify: {data!r}")
PYJSON
}

assert_ready_workflow_json() {
  local json_file="$1"
  local thread="$2"
  python3 - "$json_file" "$thread" <<'PYJSON'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
thread = sys.argv[2]
expected = f"heddle merge {thread} --preview"
context_expected_suffix = ["merge", thread, "--preview"]
argv = (data.get("recommended_action_template") or {}).get("argv_template") or []
context_ready = (
    len(argv) >= 6
    and (argv[0] == "heddle" or argv[0].endswith("/heddle"))
    and argv[1] == "--repo"
    and argv[-3:] == context_expected_suffix
)
plain_ready = (
    data.get("recommended_action") == expected
    and len(argv) == 4
    and (argv[0] == "heddle" or argv[0].endswith("/heddle"))
    and argv[1:] == context_expected_suffix
)
if data.get("status") != "completed" or data.get("thread_state") != "ready":
    raise SystemExit(f"ready command should mark the thread ready, got {data!r}")
if data.get("next_action") != data.get("recommended_action") or not (context_ready or plain_ready):
    raise SystemExit(f"ready work should point to merge preview, got {data!r}")
verify = data.get("verification") or {}
if verify.get("verified") is not True or verify.get("status") != "clean":
    raise SystemExit(f"ready work should keep repository verify clean, got {data!r}")
verify_argv = (verify.get("recommended_action_template") or {}).get("argv_template") or []
verify_context_ready = (
    len(verify_argv) >= 6
    and (verify_argv[0] == "heddle" or verify_argv[0].endswith("/heddle"))
    and verify_argv[1] == "--repo"
    and verify_argv[-3:] == context_expected_suffix
)
verify_plain_ready = (
    verify.get("recommended_action") == expected
    and len(verify_argv) == 4
    and (verify_argv[0] == "heddle" or verify_argv[0].endswith("/heddle"))
    and verify_argv[1:] == context_expected_suffix
)
if verify.get("workflow_status") != "ready" or not (verify_context_ready or verify_plain_ready):
    raise SystemExit(f"ready work should be represented as workflow_status=ready, got {data!r}")
PYJSON
}

run_verify_recommended_action_json() {
  local transcript="$1"
  local repo="$2"
  local label="$3"
  local message="$4"
  local op_id="${5:-}"
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
if template.get("agent_may_fill") is not True:
    raise SystemExit(f"agent should be allowed to fill this template: {template!r}")
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
  if [[ -n "$op_id" ]]; then
    run_json "$transcript" "$repo" "$label" --op-id "$op_id" "${action_args[@]}"
    run_json "$transcript" "$repo" "$label.replay" --op-id "$op_id" "${action_args[@]}"
  else
    run_json "$transcript" "$repo" "$label" "${action_args[@]}"
  fi
  assert_current_worktree_clean_json "$repo"
}

run_shape() {
  local shape="$1"
  local repo="$WORK_ROOT/$shape"
  local origin="${repo}.origin.git"
  local clone_path="$WORK_ROOT/$shape-clone"
  local transcript="$ARTIFACT_ROOT/$shape.jsonl"
  local final_json="$ARTIFACT_ROOT/$shape.final-verify.json"
  local clone_json="$ARTIFACT_ROOT/$shape.clone-verify.json"
  create_fixture "$repo" "$shape"
  : > "$transcript"
  python3 - "$transcript" "$(heddle_runtime_path_label)" <<'PYJSON'
import json
import sys

transcript, runtime_path = sys.argv[1:]
record = {
    "command": ["runtime-proof"],
    "output": {
        "heddle_runtime_path": runtime_path,
        "requires_git_executable": False,
        "fixture_setup": "Git is used only before the cold story to build interoperable fixture repositories.",
    },
    "exit_code": 0,
    "stdout": "",
    "stderr": "",
}
with open(transcript, "a", encoding="utf-8") as handle:
    print(json.dumps(record, sort_keys=True), file=handle)
PYJSON
  append_shape_profile_json "$transcript" "$shape"

  run_json "$transcript" "$WORK_ROOT" "$shape.00.commands" commands
  run_json "$transcript" "$WORK_ROOT" "$shape.00.schemas" schemas
  run_json "$transcript" "$WORK_ROOT" "$shape.00.clone" clone "$origin" "$clone_path"
  run_json "$transcript" "$clone_path" "$shape.00.clone-verify" verify
  (cd "$clone_path" && heddle_runtime verify --output json) > "$clone_json"
  assert_final_verify "$clone_json"

  run_json "$transcript" "$repo" "$shape.01.status" status
  run_json_expect_verify_failed "$transcript" "$repo" "$shape.02.verify-plain" verify
  if [[ "$shape" == "complex-git" ]]; then
    assert_first_run_adopt_all_json "$ARTIFACT_ROOT/$shape.02.verify-plain.json"
  else
    assert_first_run_adopt_ref_json "$ARTIFACT_ROOT/$shape.02.verify-plain.json" main
  fi
  test ! -e "$repo/.heddle"

  if [[ "$shape" == "complex-git" ]]; then
    run_json "$transcript" "$repo" "$shape.03.adopt" adopt
  else
    run_json "$transcript" "$repo" "$shape.03.adopt" adopt --ref main
  fi
  assert_clean_git_status "$repo"
  if [[ "$shape" == "complex-git" ]]; then
    run_json "$transcript" "$repo" "$shape.03.adopt-tag" adopt --ref v1.0.0
    run_json "$transcript" "$repo" "$shape.03.marker-show-tag" marker show v1.0.0
  fi
  run_json_expect_failure "$transcript" "$repo" "$shape.03.missing-thread-error" merge missing-thread --preview
  run_json "$transcript" "$repo" "$shape.04.verify-clean-after-adopt" verify
  run_json "$transcript" "$repo" "$shape.06.status-clean" status
  run_json "$transcript" "$repo" "$shape.07.doctor" doctor
  run_json "$transcript" "$repo" "$shape.08.bridge-status" bridge git status
  run_json "$transcript" "$repo" "$shape.09.reconcile-preview" bridge git reconcile --prefer heddle --ref main --preview
  run_json "$transcript" "$repo" "$shape.10.thread-list" thread list
  run_json "$transcript" "$repo" "$shape.11.thread-show" thread show
  run_json "$transcript" "$repo" "$shape.12.workspace-show" workspace show

  make_agent_capture_edit "$repo" "$shape"
  run_json "$transcript" "$repo" "$shape.13.diff-capture" diff
  if [[ "$shape" != "small-app" ]]; then
    run_json "$transcript" "$repo" "$shape.13.diff-capture-stat" diff --stat
    run_json "$transcript" "$repo" "$shape.13.diff-capture-name-only" diff --name-only
  fi
  run_json "$transcript" "$repo" "$shape.14.capture" capture -m "agent capture $shape" --confidence 0.86
  run_json "$transcript" "$repo" "$shape.15.checkpoint" checkpoint -m "agent checkpoint $shape"
  run_json "$transcript" "$repo" "$shape.16.push-checkpoint" push "$origin"
  run_json "$transcript" "$repo" "$shape.17.fetch" fetch "$origin"
  assert_fetch_reports_verification_json "$ARTIFACT_ROOT/$shape.17.fetch.json"
  run_json "$transcript" "$repo" "$shape.18.pull" pull "$origin"
  assert_clean_git_status "$repo"

  make_agent_commit_edit "$repo" "$shape" first
  run_json "$transcript" "$repo" "$shape.19.diff-commit" diff
  if [[ "$shape" != "small-app" ]]; then
    run_json "$transcript" "$repo" "$shape.19.diff-commit-stat" diff --stat
    run_json "$transcript" "$repo" "$shape.19.diff-commit-name-only" diff --name-only
  fi
  run_json "$transcript" "$repo" "$shape.20.status-dirty-template" status
  run_json_expect_verify_failed "$transcript" "$repo" "$shape.20.verify-dirty-template" verify
  local template_op_id
  case "$shape" in
    small-app) template_op_id="550e8400-e29b-41d4-a716-446655440010" ;;
    large-rust) template_op_id="550e8400-e29b-41d4-a716-446655440011" ;;
    complex-git) template_op_id="550e8400-e29b-41d4-a716-446655440012" ;;
  esac
  run_verify_recommended_action_json \
    "$transcript" \
    "$repo" \
    "$shape.20.commit-from-template" \
    "agent verify cold flow $shape" \
    "$template_op_id"
  assert_local_ahead_verified_json "$ARTIFACT_ROOT/$shape.20.commit-from-template.json"
  run_json "$transcript" "$repo" "$shape.21.undo" undo
  assert_clean_git_status "$repo"
  make_agent_commit_edit "$repo" "$shape" after_undo
  run_json "$transcript" "$repo" "$shape.22.commit-after-undo" commit -m "agent verify cold flow after undo $shape" --confidence 0.9
  assert_local_ahead_verified_json "$ARTIFACT_ROOT/$shape.22.commit-after-undo.json"
  run_json "$transcript" "$repo" "$shape.23.push-commit" push "$origin"
  run_json "$transcript" "$repo" "$shape.24.ready" ready
  assert_clean_git_status "$repo"
  run_json "$transcript" "$repo" "$shape.25.blame" blame "$(agent_blame_target_for_shape "$shape")"
  run_json "$transcript" "$repo" "$shape.26.log" log

  local feature_thread="feature-$shape"
  local feature_path="$WORK_ROOT/$shape-isolated"
  run_json "$transcript" "$repo" "$shape.27.start" start "$feature_thread" --path "$feature_path" --workspace solid
  make_agent_isolated_edit "$feature_path" "$shape"
  run_json "$transcript" "$feature_path" "$shape.28.diff-isolated" diff
  if [[ "$shape" != "small-app" ]]; then
    run_json "$transcript" "$feature_path" "$shape.28.diff-isolated-stat" diff --stat
    run_json "$transcript" "$feature_path" "$shape.28.diff-isolated-name-only" diff --name-only
  fi
  run_json "$transcript" "$feature_path" "$shape.29.capture-isolated" capture -m "isolated agent capture $shape" --confidence 0.84
  run_json "$transcript" "$feature_path" "$shape.30.ready-isolated" ready
  assert_ready_workflow_json "$ARTIFACT_ROOT/$shape.30.ready-isolated.json" "$feature_thread"
  run_json "$transcript" "$repo" "$shape.31.merge-preview" merge "$feature_thread" --preview --with-diff
  assert_merge_preview_points_to_ship_json "$ARTIFACT_ROOT/$shape.31.merge-preview.json" "$feature_thread"
  run_json "$transcript" "$repo" "$shape.32.thread-show-feature" thread show "$feature_thread"
  run_json "$transcript" "$repo" "$shape.33.ship-feature" ship --thread "$feature_thread" --no-push
  assert_local_ahead_verified_json "$ARTIFACT_ROOT/$shape.33.ship-feature.json"
  run_json "$transcript" "$repo" "$shape.34.push-feature" push "$origin"

  run_json "$transcript" "$repo" "$shape.final-verify" verify
  assert_final_verify "$final_json"
  assert_transcript_claims "$transcript"
  assert_shape_transcript_claims_json "$transcript" "$shape"
  assert_no_literal_round_leak "$ARTIFACT_ROOT"
}

for shape in small-app large-rust complex-git; do
  run_shape "$shape"
done

assert_no_literal_round_leak "$ARTIFACT_ROOT"

echo "verify cold-flow agent transcripts: $ARTIFACT_ROOT"
