#!/usr/bin/env bash
# Git round-trip controls and automatic integration repro for issue #1895.
# Usage: scripts/repro-1895.sh /absolute/path/to/heddle [automatic|single|manual]
set -euo pipefail
heddle_bin=$1
mode=${2:-automatic}
export TMPDIR=/home/scratch
export HEDDLE_HOME
HEDDLE_HOME=$(mktemp -d)
export HEDDLE_PRINCIPAL_NAME='Roundtrip Test'
export HEDDLE_PRINCIPAL_EMAIL='roundtrip@example.com'
export HEDDLE_FSMONITOR=off
repro_root=$(mktemp -d)
echo "Repro: $repro_root ($mode)"
mkdir "$repro_root/source"
cd "$repro_root/source"
git init -b main
git config user.name "$HEDDLE_PRINCIPAL_NAME"
git config user.email "$HEDDLE_PRINCIPAL_EMAIL"
printf 'def alpha():\n    return "alpha"\n\ndef beta():\n    return "beta"\n' > app.py
git add app.py
git commit -m base
"$heddle_bin" init --no-harness-install
"$heddle_bin" start a --path "$repro_root/a"
if [[ $mode != single ]]; then
    "$heddle_bin" start b --path "$repro_root/b"
fi
cd "$repro_root/a"
sed -i 's/return "alpha"/return "alpha edited"/' app.py
"$heddle_bin" capture -m 'edit alpha'
if [[ $mode != single ]]; then
    cd "$repro_root/b"
    if [[ $mode == manual ]]; then
        sed -i 's/return "alpha"/return "alpha conflicting"/' app.py
    else
        sed -i 's/return "beta"/return "beta edited"/' app.py
    fi
    "$heddle_bin" capture -m 'edit beta or conflict'
fi
cd "$repro_root/a"
"$heddle_bin" ready
"$heddle_bin" land
if [[ $mode != single ]]; then
    cd "$repro_root/b"
    if [[ $mode == manual ]]; then
        if "$heddle_bin" ready; then
            echo 'Expected ready to report a conflict' >&2
            exit 1
        fi
        printf 'def alpha():\n    return "alpha edited"\n\ndef beta():\n    return "beta edited"\n' > app.py
        "$heddle_bin" resolve app.py
        "$heddle_bin" continue
    fi
    "$heddle_bin" ready
    "$heddle_bin" land
fi
cd "$repro_root/source"
"$heddle_bin" verify
git notes --ref=heddle show HEAD > "$repro_root/note-before-push.json"
git init --bare -b main "$repro_root/remote.git"
git remote add origin "$repro_root/remote.git"
"$heddle_bin" push
git notes --ref=heddle show HEAD > "$repro_root/note-after-push.json"
git show -s --format=raw HEAD
git clone "$repro_root/remote.git" "$repro_root/plain"
git -C "$repro_root/plain" fsck
cd "$repro_root"
"$heddle_bin" --output json clone --source git "$repro_root/remote.git" "$repro_root/cloned"
cd "$repro_root/cloned"
"$heddle_bin" verify
cat app.py
