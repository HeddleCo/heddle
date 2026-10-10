# SPDX-License-Identifier: Apache-2.0
# Shared, fixed limits for run.sh and smoke.sh. This file is sourced, not executed.
prepare_image() {
    engine=docker
    command -v "$engine" >/dev/null 2>&1 || {
        echo "$engine is unavailable; no image was built, pulled, or run" >&2
        exit 127
    }
    case "$1" in
        ''|-*) echo 'An existing local image reference is required' >&2; exit 2 ;;
    esac
    # Resolve once, then run that immutable local image ID. Never pull implicitly.
    image_id=$("$engine" image inspect --format '{{.Id}}' "$1") || {
        echo 'Image is not available locally; build it separately before running' >&2
        exit 1
    }
    case "$image_id" in
        sha256:*) ;;
        *) echo 'Container engine returned an invalid image ID' >&2; exit 1 ;;
    esac
}

run_image() {
    exec "$engine" run \
        --pull=never --rm \
        --user=65532:65532 --read-only \
        --cap-drop=ALL --security-opt=no-new-privileges:true \
        --pids-limit=64 --memory=1g --memory-swap=1g --cpus=1 \
        --ulimit=nofile=128:128 --ulimit=nproc=64:64 \
        --ulimit=fsize=100663296:100663296 --ulimit=core=0:0 \
        --shm-size=16m \
        --tmpfs=/tmp:rw,noexec,nosuid,nodev,size=512m,mode=0700,uid=65532,gid=65532 \
        "$@"
}
