#!/usr/bin/env bash
# Publish all 17 OSS Heddle crates at 0.2.0 to crates.io in topological order.
#
# - heddle-objects has already been published; this script skips it.
# - Upgrades (existing 0.0.0 stubs) publish quickly with 60s gaps.
# - New crates trigger the 1-per-10-min rate limit, so they're paced 12 min apart.
# - Wrapped in `caffeinate -i` so the Mac doesn't sleep during the ~2.5h run.
# - Failures are retried twice with 5-min backoff; if still failing, the script
#   stops so you can investigate.

set -u

if ! command -v caffeinate >/dev/null 2>&1; then
  echo "caffeinate not found (macOS expected). Falling through anyway." >&2
fi

CRATES_DIR="$HOME/dev/HeddleCo/heddle"
LOG="$CRATES_DIR/publish-0.2.0.log"
exec > >(tee -a "$LOG") 2>&1

cd "$CRATES_DIR" || { echo "FATAL: cannot cd to $CRATES_DIR"; exit 1; }

# Verify crates.io creds are loaded
if [[ ! -s "$HOME/.cargo/credentials.toml" ]]; then
  echo "FATAL: no crates.io credentials in ~/.cargo/credentials.toml" >&2
  exit 1
fi

# (kind, name) — kind=upgrade publishes over an existing 0.0.0 stub (fast);
# kind=new is a first-time publish and triggers the 10-min rate limit.
PUBLISHES=(
  "upgrade:heddle-crypto"
  "upgrade:heddle-refs"
  "upgrade:heddle-oplog"
  "new:heddle-proto"
  "new:heddle-semantic"
  "new:heddle-devtools"
  "new:heddle-grpc"
  "new:heddle-review"
  "new:heddle-state-review"
  "upgrade:heddle-repo"
  "new:heddle-mount"
  "new:heddle-ingest"
  "new:heddle-cli-shared"
  "new:weft-client-shim"
  "new:heddle-daemon"
  "new:heddle-cli"
)

UPGRADE_GAP_SECS=60         # 1 min between upgrades
NEW_GAP_SECS=720            # 12 min between new-crate publishes
RETRY_BACKOFF_SECS=300      # 5 min between retries
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

run_all() {
  local i=0
  local total=${#PUBLISHES[@]}
  for entry in "${PUBLISHES[@]}"; do
    i=$((i+1))
    local kind="${entry%%:*}"
    local name="${entry##*:}"
    echo ""
    echo "############ [$i/$total] $kind: $name ############"
    if ! publish_one "$name"; then
      echo "FATAL: stopping the run; check the log above."
      return 1
    fi
    if (( i < total )); then
      local gap
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
  return 0
}

echo "=== publish-0.2.0.sh starting at $(date -u) ==="
echo "=== logging to $LOG ==="
echo "=== ${#PUBLISHES[@]} crates to publish ==="
echo ""

run_all
