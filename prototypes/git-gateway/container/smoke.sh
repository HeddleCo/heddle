#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
set -eu
if [ "$#" -ne 1 ]; then
    echo "Usage: $0 EXISTING_LOCAL_IMAGE" >&2
    exit 2
fi
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$script_dir/runtime.sh"
prepare_image "$1"
# No fixture, credentials, daemon, config mount, network, published port, or build.
run_image --network=none --interactive --entrypoint=python3 "$image_id" - < "$script_dir/smoke.py"
