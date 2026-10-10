#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Offline Python-free startup-denial check; no platform integration is claimed.
set -eu
[ "$#" -eq 1 ] || { echo "Usage: $0 EXISTING_LOCAL_IMAGE" >&2; exit 2; }
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$script_dir/../../container/runtime.sh"
prepare_image "$1"
run_image --network=none --interactive --entrypoint=/bin/sh "$image_id" -s <<'CHECK'
set -eu
[ "$(id -u)" -eq 65532 ]
[ -z "${GATEWAY_DEMO_CONFIG_JSON:-}" ]
! command -v python3
if /usr/local/bin/gateway_host >/tmp/startup.log 2>&1; then exit 1; fi
grep -q 'Native gateway unavailable' /tmp/startup.log
printf '%s\n' 'Rust host loads; missing configuration fails closed. No live platform integration tested.'
CHECK
