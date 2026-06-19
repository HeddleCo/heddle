#!/usr/bin/env bash
# Resume publish-0.2.0 from heddle-grpc onward.
#
# The first run stopped at heddle-grpc because its build.rs was reading
# proto from the workspace root, which `cargo publish` does not package.
# The fix lives in commit that ships crates/grpc/proto/heddle/v1/service.proto
# + a build.rs that looks at CARGO_MANIFEST_DIR/proto.
#
# Run with `caffeinate -i` to keep the Mac awake for the ~2-hour run.

set -u

CRATES_DIR="$HOME/dev/HeddleCo/heddle"
LOG="$CRATES_DIR/publish-0.2.0.log"
exec > >(tee -a "$LOG") 2>&1

cd "$CRATES_DIR" || { echo "FATAL: cannot cd to $CRATES_DIR"; exit 1; }

PUBLISHES=(
  "new:heddle-grpc"
  "new:heddle-state-review"
  "upgrade:heddle-repo"
  "new:heddle-mount"
  "new:heddle-ingest"
  "new:heddle-cli-shared"
  "new:heddle-daemon"
  "new:heddle-cli"
)

UPGRADE_GAP_SECS=60
NEW_GAP_SECS=720
RETRY_BACKOFF_SECS=300
MAX_RETRIES=2

publish_one() {
  local name="$1"
  local attempt=0
  while true; do
    echo ""
    echo "=== $(date -u) — publishing $name (attempt $((attempt+1))) ==="
    if cargo publish -p "$name" --allow-dirty; then
      echo "=== $(date -u) — $name OK ==="
      return 0
    fi
    attempt=$((attempt+1))
    if (( attempt > MAX_RETRIES )); then
      echo "=== $(date -u) — $name FAILED after $((MAX_RETRIES+1)) attempts; STOPPING ==="
      return 1
    fi
    echo "=== $(date -u) — $name failed; backing off ${RETRY_BACKOFF_SECS}s ==="
    sleep "$RETRY_BACKOFF_SECS"
  done
}

echo ""
echo "############ RESUME publish-0.2.0 at $(date -u) ############"
echo "############ ${#PUBLISHES[@]} crates remaining ############"

i=0
total=${#PUBLISHES[@]}
for entry in "${PUBLISHES[@]}"; do
  i=$((i+1))
  kind="${entry%%:*}"
  name="${entry##*:}"
  echo ""
  echo "############ [$i/$total] $kind: $name ############"
  if ! publish_one "$name"; then
    echo "FATAL: stopping the run; check the log above."
    exit 1
  fi
  if (( i < total )); then
    if [[ "$kind" == "new" ]]; then
      gap="$NEW_GAP_SECS"
    else
      gap="$UPGRADE_GAP_SECS"
    fi
    echo "--- $(date -u) — sleeping ${gap}s before next publish ---"
    sleep "$gap"
  fi
done

echo ""
echo "############ ALL DONE ############"
echo "$(date -u): published $total crates."
