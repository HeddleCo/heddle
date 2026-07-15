#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

build_log=$(mktemp)
trap 'rm -f "$build_log"' EXIT
cargo build --locked -p heddle-cli --message-format=json >"$build_log"

heddle_bin=
worker_bin=
while IFS=$'\t' read -r name path; do
  case "$name" in
    heddle) heddle_bin=$path ;;
    heddle-fuse-worker) worker_bin=$path ;;
  esac
done < <(python3 - "$build_log" <<'PY'
import json
import sys

artifacts = {}
with open(sys.argv[1]) as messages:
    for line in messages:
        try:
            message = json.loads(line)
        except ValueError:
            continue
        name = message.get("target", {}).get("name")
        executable = message.get("executable")
        if message.get("reason") == "compiler-artifact" and executable:
            artifacts[name] = executable

for name in ("heddle", "heddle-fuse-worker"):
    if name in artifacts:
        print(f"{name}\t{artifacts[name]}")
PY
)

if [ ! -x "$heddle_bin" ]; then
  echo "ERROR: default heddle-cli build did not produce an executable heddle binary" >&2
  exit 1
fi
if [ ! -x "$worker_bin" ]; then
  echo "ERROR: default heddle-cli build did not produce an executable heddle-fuse-worker binary" >&2
  exit 1
fi

HEDDLE_BIN="$heddle_bin" bash scripts/verify-cold-flow-human.sh
HEDDLE_BIN="$heddle_bin" bash scripts/verify-cold-flow-agent.sh
"$heddle_bin" --repo "$repo_root" doctor schemas
