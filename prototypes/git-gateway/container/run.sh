#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
set -eu
if [ "$#" -ne 2 ]; then
    echo "Usage: $0 CONFIG_DIRECTORY EXISTING_LOCAL_IMAGE" >&2
    exit 2
fi
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$script_dir/runtime.sh"
config=$(CDPATH= cd -- "$1" && pwd -P)
case "$config" in
    *,*) echo 'Config directory cannot contain a comma (mount option separator)' >&2; exit 2 ;;
esac
[ -f "$config/host.json" ] || { echo 'Config directory must contain host.json' >&2; exit 2; }
prepare_image "$2"
# A private bridge permits the configured HTTPS reads. Nothing is published to
# the host; gateway.host still binds only 127.0.0.1 in this network namespace.
# Disabling recursive bind mounts prevents writable nested mounts in /config.
run_image --network=bridge \
    --mount="type=bind,src=$config,dst=/config,readonly,bind-recursive=disabled" \
    "$image_id"
