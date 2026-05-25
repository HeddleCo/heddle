#!/usr/bin/env bash
# Bridge Migration Test: Pull 10 git repos into Heddle, verify fidelity, compare storage.
set -euo pipefail

HEDDLE="$(cd "$(dirname "$0")/.." && pwd)/target/release/heddle"
WORKDIR="/tmp/heddle-bridge-test-$$"
RESULTS="$WORKDIR/results.csv"

if [[ ! -x "$HEDDLE" ]]; then
  echo "ERROR: heddle binary not found at $HEDDLE — run 'cargo build --release' first"
  exit 1
fi

mkdir -p "$WORKDIR"
echo "repo,git_commits,git_branches,git_tags,git_size_kb,heddle_size_kb,ratio" > "$RESULTS"

# 10 test repositories
declare -a REPOS=(
  "github/git-sizer"
  "tj/commander.js"
  "isaacs/node-glob"
  "nickel-org/nickel.rs"
  "sharkdp/hyperfine"
  "BurntSushi/ripgrep"
  "charmbracelet/glow"
  "mdomke/git-semver"
  "benhoyt/goawk"
)
# git/git handled separately (shallow clone)

PASS=0
FAIL=0
ERRORS=""

log() { echo -e "\n\033[1;36m==> $1\033[0m"; }
ok()  { echo "  \033[32m✓ $1\033[0m"; }
err() { echo "  \033[31m✗ $1\033[0m"; ERRORS="$ERRORS\n  - $1"; ((FAIL++)) || true; }

verify_repo() {
  local name="$1"
  local git_mirror="$2"
  local heddle_dir="$3"
  local import_method="$4"  # "pull" or "import"

  # --- Verify commit count ---
  # Count only commits reachable from refs/heads/* and refs/tags/* (what heddle imports)
  local git_commits
  if [[ "$import_method" == "shallow" ]]; then
    git_commits=$(git -C "$git_mirror" rev-list --all --count 2>/dev/null || echo 0)
  else
    git_commits=$(git -C "$git_mirror" rev-list $(git -C "$git_mirror" for-each-ref --format='%(objectname)' refs/heads/ refs/tags/) --count 2>/dev/null || echo 0)
  fi

  # Use heddle import stats from the JSON output we captured, or count via import --output json
  local heddle_import_json="$WORKDIR/${name}_import.json"
  local heddle_states=0
  if [[ -f "$heddle_import_json" ]]; then
    heddle_states=$(python3 -c "
import json
data = json.load(open('$heddle_import_json'))
print(data.get('commits_imported', 0))
" 2>/dev/null || echo 0)
  fi

  if [[ "$git_commits" -eq "$heddle_states" ]]; then
    ok "Commit count: $git_commits"
  else
    err "$name: commit count mismatch — git=$git_commits heddle=$heddle_states"
  fi

  # --- Verify branches -> threads ---
  local git_branches_file="$WORKDIR/${name}_git_branches.txt"
  local heddle_threads_file="$WORKDIR/${name}_heddle_threads.txt"

  if [[ "$import_method" == "shallow" ]]; then
    # For shallow non-mirror clones, local branches are remote tracking
    git -C "$git_mirror" for-each-ref --format='%(refname:short)' refs/heads/ | sort > "$git_branches_file"
    # If empty, try remote branches
    if [[ ! -s "$git_branches_file" ]]; then
      git -C "$git_mirror" for-each-ref --format='%(refname:lstrip=3)' refs/remotes/origin/ | grep -v HEAD | sort > "$git_branches_file"
    fi
  else
    git -C "$git_mirror" for-each-ref --format='%(refname:short)' refs/heads/ | sort > "$git_branches_file"
  fi

  (cd "$heddle_dir" && "$HEDDLE" --output json thread list 2>/dev/null | python3 -c "
import sys, json
data = json.load(sys.stdin)
threads = data.get('threads', data) if isinstance(data, dict) else data
if isinstance(threads, list):
    for t in threads:
        name = t.get('name', '') if isinstance(t, dict) else str(t)
        print(name)
" | sort > "$heddle_threads_file") || true

  local git_branch_count heddle_thread_count
  git_branch_count=$(wc -l < "$git_branches_file" | tr -d ' ')
  heddle_thread_count=$(wc -l < "$heddle_threads_file" | tr -d ' ')

  local branch_diff
  branch_diff=$(diff "$git_branches_file" "$heddle_threads_file" 2>/dev/null || true)
  if [[ -z "$branch_diff" ]]; then
    ok "Branches match: $git_branch_count"
  else
    err "$name: branch mismatch — git=$git_branch_count heddle=$heddle_thread_count"
    echo "$branch_diff" | head -8 | sed 's/^/    /'
  fi

  # --- Verify tags -> markers ---
  local git_tags_file="$WORKDIR/${name}_git_tags.txt"
  local heddle_markers_file="$WORKDIR/${name}_heddle_markers.txt"

  git -C "$git_mirror" for-each-ref --format='%(refname:short)' refs/tags/ | sort > "$git_tags_file"
  (cd "$heddle_dir" && "$HEDDLE" --output json marker list 2>/dev/null | python3 -c "
import sys, json
data = json.load(sys.stdin)
markers = data.get('markers', data) if isinstance(data, dict) else data
if isinstance(markers, list):
    for m in markers:
        name = m.get('name', '') if isinstance(m, dict) else str(m)
        print(name)
" | sort > "$heddle_markers_file") || true

  local git_tag_count heddle_marker_count
  git_tag_count=$(wc -l < "$git_tags_file" | tr -d ' ')
  heddle_marker_count=$(wc -l < "$heddle_markers_file" | tr -d ' ')

  local tag_diff
  tag_diff=$(diff "$git_tags_file" "$heddle_markers_file" 2>/dev/null || true)
  if [[ -z "$tag_diff" ]]; then
    ok "Tags match: $git_tag_count"
  else
    err "$name: tag mismatch — git=$git_tag_count heddle=$heddle_marker_count"
    echo "$tag_diff" | head -8 | sed 's/^/    /'
  fi

  # --- Measure storage ---
  local git_size_kb heddle_size_kb
  if [[ "$import_method" == "shallow" ]]; then
    git_size_kb=$(du -sk "$git_mirror/.git" | cut -f1)
  else
    git_size_kb=$(du -sk "$git_mirror" | cut -f1)
  fi
  heddle_size_kb=$(du -sk "$heddle_dir/.heddle" 2>/dev/null | cut -f1 || echo 0)

  local ratio
  if [[ "$git_size_kb" -gt 0 ]]; then
    ratio=$(python3 -c "print(f'{$heddle_size_kb / $git_size_kb:.2f}')")
  else
    ratio="N/A"
  fi

  ok "Storage: git=${git_size_kb}KB heddle=${heddle_size_kb}KB ratio=${ratio}x"

  # --- Round-trip export ---
  echo "  Exporting back to git..."
  if (cd "$heddle_dir" && "$HEDDLE" bridge git export 2>&1); then
    ok "Round-trip export succeeded"
  else
    err "$name: heddle bridge git export failed"
  fi

  # Record results
  echo "$name,$git_commits,$git_branch_count,$git_tag_count,$git_size_kb,$heddle_size_kb,$ratio" >> "$RESULTS"
  ((PASS++)) || true
}

test_repo() {
  local slug="$1"
  local name="${slug##*/}"
  local git_mirror="$WORKDIR/${name}.git"
  local heddle_dir="$WORKDIR/${name}-heddle"

  log "Testing $slug"

  # --- Step 1: Clone git mirror for baseline ---
  echo "  Cloning git mirror..."
  git clone --mirror --quiet "https://github.com/${slug}.git" "$git_mirror" 2>&1

  # --- Step 2: Pull into Heddle and capture import stats ---
  echo "  Pulling into Heddle..."
  mkdir -p "$heddle_dir"
  (cd "$heddle_dir" && "$HEDDLE" init --quiet 2>/dev/null || "$HEDDLE" init 2>/dev/null)

  # Use import --path with the mirror (more reliable than pull for verification,
  # and we get the JSON import stats directly)
  local import_output
  if import_output=$(cd "$heddle_dir" && "$HEDDLE" --output json bridge git import --path "$git_mirror" 2>&1); then
    echo "$import_output" > "$WORKDIR/${name}_import.json"
    ok "Import succeeded: $import_output"
  else
    echo "$import_output"
    err "$name: heddle bridge git import failed"
    return
  fi

  verify_repo "$name" "$git_mirror" "$heddle_dir" "mirror"
}

# Run tests for standard repos
for repo in "${REPOS[@]}"; do
  test_repo "$repo" || true
done

# Handle git/git separately (shallow clone)
log "Testing git/git (shallow --depth 50)"
GIT_SHALLOW="$WORKDIR/git-shallow"
GIT_GALEED="$WORKDIR/git-heddle"
git clone --depth 50 --no-single-branch --quiet "https://github.com/git/git.git" "$GIT_SHALLOW" 2>&1
mkdir -p "$GIT_GALEED"
(cd "$GIT_GALEED" && "$HEDDLE" init --quiet 2>/dev/null || "$HEDDLE" init 2>/dev/null)
import_output=$(cd "$GIT_GALEED" && "$HEDDLE" --output json bridge git import --path "$GIT_SHALLOW" 2>&1) && {
  echo "$import_output" > "$WORKDIR/git_import.json"
  ok "git/git import succeeded"
} || {
  echo "$import_output" > "$WORKDIR/git_import.json"
  err "git: heddle bridge git import failed: $import_output"
}

verify_repo "git" "$GIT_SHALLOW" "$GIT_GALEED" "shallow"

# --- Summary ---
log "Results"
echo ""
column -t -s',' "$RESULTS" 2>/dev/null || cat "$RESULTS"
echo ""
echo "Passed: $PASS  Failed: $FAIL"
if [[ -n "$ERRORS" ]]; then
  echo -e "\nErrors:$ERRORS"
fi
echo ""
echo "Working directory: $WORKDIR"
echo "Results CSV: $RESULTS"
