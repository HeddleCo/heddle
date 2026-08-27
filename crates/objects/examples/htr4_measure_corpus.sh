#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

root=${1:?usage: htr4_measure_corpus.sh OUTPUT_DIRECTORY}
mkdir -p "$root"

emit_data() {
    local value=$1
    printf 'data %d\n%s' "${#value}" "$value"
}

tiny="$root/many-tiny-trees.git"
git init --bare "$tiny"
{
    printf 'commit refs/heads/main\n'
    printf 'committer HTR4 Measurement <measure@example.com> 1700000000 +0000\n'
    emit_data '12,000 unique one-file subtrees'
    printf '\n'
    for ((entry = 0; entry < 12000; entry++)); do
        printf -v content 'tiny-%05d' "$entry"
        printf 'M 100644 inline dir-%05d/index.js\n' "$entry"
        emit_data "$content"
        printf '\n'
    done
    printf 'done\n'
} | git -C "$tiny" fast-import --quiet
git -C "$tiny" gc --prune=now

fanout="$root/huge-fanout.git"
git init --bare "$fanout"
{
    for ((revision = 0; revision < 64; revision++)); do
        printf 'commit refs/heads/main\n'
        printf 'committer HTR4 Measurement <measure@example.com> %d +0000\n' "$((1700100000 + revision))"
        printf -v message '10,000-sibling root revision %d' "$revision"
        emit_data "$message"
        printf '\n'
        if ((revision == 0)); then
            for ((entry = 0; entry < 10000; entry++)); do
                printf -v content 'fanout-%05d-r00' "$entry"
                printf 'M 100644 inline file-%05d.txt\n' "$entry"
                emit_data "$content"
                printf '\n'
            done
        else
            entry=$(((revision * 157) % 10000))
            printf -v content 'fanout-%05d-r%02d' "$entry" "$revision"
            printf 'M 100644 inline file-%05d.txt\n' "$entry"
            emit_data "$content"
            printf '\n'
        fi
    done
    printf 'done\n'
} | git -C "$fanout" fast-import --quiet
git -C "$fanout" gc --prune=now

deep="$root/deep-vendored.git"
git init --bare "$deep"
{
    printf 'commit refs/heads/main\n'
    printf 'committer HTR4 Measurement <measure@example.com> 1700200000 +0000\n'
    emit_data '32 vendored dependency chains, each 32 packages deep'
    printf '\n'
    for ((chain = 0; chain < 32; chain++)); do
        path="vendor/chain-$(printf '%02d' "$chain")"
        for ((depth = 0; depth < 32; depth++)); do
            path="$path/node_modules/pkg-$(printf '%02d' "$depth")"
        done
        printf -v content 'chain-%02d-leaf' "$chain"
        printf 'M 100644 inline %s/index.js\n' "$path"
        emit_data "$content"
        printf '\n'
    done
    printf 'done\n'
} | git -C "$deep" fast-import --quiet
git -C "$deep" gc --prune=now

printf '%s\n' "$tiny" "$fanout" "$deep"
