#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  echo "usage: $0 <fresh-work-dir> [semver|ripgrep|curl ...]" >&2
  echo "default corpora: semver ripgrep" >&2
}

if (( $# < 1 )); then
  usage
  exit 64
fi

run_root=$1
shift
if (( $# == 0 )); then
  set -- semver ripgrep
fi

if [[ -e "$run_root" ]]; then
  echo "benchmark work directory must not exist: $run_root" >&2
  exit 73
fi

for command in cargo git jq /usr/bin/zstd; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "required command is unavailable: $command" >&2
    exit 69
  fi
done

source_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mkdir -p "$run_root"
run_root=$(cd "$run_root" && pwd)
benchmark_tmp="$run_root/tmp"
benchmark_target=${CARGO_TARGET_DIR:-"$run_root/target"}
if [[ "$benchmark_target" != /* ]]; then
  echo "CARGO_TARGET_DIR must be absolute when supplied: $benchmark_target" >&2
  exit 64
fi
mkdir -p "$benchmark_tmp" "$benchmark_target"

export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null

# The heddle binary ships normally; the storage benchmark is inert
# archived source (experiments/native-lineage-solid/, no Cargo.toml),
# compiled here straight against the release artifacts the build above
# just produced.
TMPDIR="$benchmark_tmp" CARGO_TARGET_DIR="$benchmark_target" \
  cargo build \
    --manifest-path "$source_root/Cargo.toml" \
    --locked \
    --release \
    -p heddle-cli \
    --bin heddle \
    --features semantic,zstd

heddle_bin="$benchmark_target/release/heddle"
benchmark_deps="$benchmark_target/release/deps"
benchmark_bin="$benchmark_target/release/native_lineage_solid"

rustc_externs=()
for bench_crate in anyhow objects semantic serde serde_json; do
  candidates=("$benchmark_deps"/lib"$bench_crate"-*.rlib)
  if [[ ! -e "${candidates[0]}" ]]; then
    echo "missing benchmark dependency artifact: $bench_crate" >&2
    exit 69
  fi
  benchmark_rlib=$(ls -t "${candidates[@]}" | head -n1)
  rustc_externs+=(--extern "$bench_crate=$benchmark_rlib")
done

rustc --edition 2024 -O "${rustc_externs[@]}" \
  -L "dependency=$benchmark_deps" \
  -o "$benchmark_bin" \
  "$source_root/experiments/native-lineage-solid/native_lineage_solid.rs"

corpus_details() {
  case "$1" in
    semver)
      printf '%s\t%s\n' \
        'https://github.com/maykonlf/semver-cli.git' \
        '4fea7f0d5a1c9fca85f764c02953643d7fd45b27'
      ;;
    ripgrep)
      printf '%s\t%s\n' \
        'https://github.com/BurntSushi/ripgrep.git' \
        '3fce3b5bb0236da2df6d99672afb8a719642eca7'
      ;;
    curl)
      printf '%s\t%s\n' \
        'https://github.com/curl/curl.git' \
        'f5378b88a974e565f767f2a041972aa942a69c5d'
      ;;
    *)
      echo "unknown corpus: $1" >&2
      usage
      exit 64
      ;;
  esac
}

run_corpus() {
  local corpus=$1
  local details url revision git_dir checkout output
  details=$(corpus_details "$corpus")
  IFS=$'\t' read -r url revision <<<"$details"
  git_dir="$run_root/$corpus.git"
  checkout="$run_root/$corpus"
  output="$run_root/$corpus-results"

  git init --bare --quiet "$git_dir"
  git --git-dir="$git_dir" fetch --quiet --no-tags "$url" \
    "$revision:refs/heads/storage-bench"
  git --git-dir="$git_dir" symbolic-ref HEAD refs/heads/storage-bench
  git --git-dir="$git_dir" reflog expire --expire=now --all
  git --git-dir="$git_dir" repack -adf --window=250 --depth=50
  git --git-dir="$git_dir" prune-packed
  git clone --quiet --no-local "$git_dir" "$checkout"

  "$heddle_bin" --output json adopt "$checkout" --ref storage-bench \
    >"$run_root/$corpus-adopt.json"

  TMPDIR="$benchmark_tmp" "$benchmark_bin" "$checkout" "$git_dir" "$output"
}

declare -A selected=()
for corpus in "$@"; do
  if [[ -n "${selected[$corpus]:-}" ]]; then
    echo "duplicate corpus: $corpus" >&2
    exit 64
  fi
  selected[$corpus]=1
  run_corpus "$corpus"
done

echo
echo -e 'corpus\tobjects\tbefore/git\tafter/git\tbyte-identical'
for corpus in "$@"; do
  jq -r --arg corpus "$corpus" '
    [
      $corpus,
      .object_counts.total,
      .regated_total.physical_before_to_git_pack,
      .regated_total.physical_after_to_git_pack,
      .compact.byte_identical
    ] | @tsv
  ' "$run_root/$corpus-results/results.json"
done
