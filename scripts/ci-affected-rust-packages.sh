#!/usr/bin/env bash
set -euo pipefail

# Select workspace packages affected by a changed-path list.
#
# The CI Rust lane uses this to avoid rebuilding and testing the whole
# workspace when a PR or push only touches a leaf crate. Package selection is
# intentionally fail-closed: workspace-wide inputs or unknown build-relevant
# paths select the whole workspace. Crate paths select the owning crate plus
# every workspace crate that depends on it, including dev-dependencies, because
# `cargo test` and `cargo clippy --all-targets` compile test targets too.

DOC_PACKAGE=heddle-cli
CLI_PACKAGE=heddle-cli

changed_paths=
metadata_json=
github_output=
select_all=false

usage() {
  cat <<'EOF'
usage: ci-affected-rust-packages.sh (--all | --changed-paths PATH) [--metadata-json PATH] [--github-output PATH]
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --all)
      select_all=true
      shift
      ;;
    --changed-paths)
      changed_paths=${2:?--changed-paths requires a path}
      shift 2
      ;;
    --metadata-json)
      metadata_json=${2:?--metadata-json requires a path}
      shift 2
      ;;
    --github-output)
      github_output=${2:?--github-output requires a path}
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ "$select_all" != "true" ] && [ -z "$changed_paths" ]; then
  echo "either --all or --changed-paths is required" >&2
  usage >&2
  exit 2
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required to read cargo metadata" >&2
  exit 2
fi

if [ -n "$metadata_json" ]; then
  metadata=$(<"$metadata_json")
else
  metadata=$(cargo metadata --locked --format-version 1 --no-deps)
fi

declare -a package_names=()
declare -A package_by_dir=()
declare -A package_exists=()
declare -A deps_by_package=()
declare -A reverse_deps=()
declare -A direct_packages=()
declare -A selected_packages=()
declare -a reasons=()

while IFS=$'\t' read -r name crate_dir deps; do
  [ -n "$name" ] || continue
  package_names+=("$name")
  package_by_dir["$crate_dir"]=$name
  package_exists["$name"]=1
  deps_by_package["$name"]=${deps:-}
done < <(
  jq -r '
    . as $m
    | ($m.workspace_root | gsub("\\\\"; "/")) as $root
    | $m.workspace_members[] as $id
    | $m.packages[]
    | select(.id == $id)
    | [
        .name,
        (.manifest_path | gsub("\\\\"; "/") | sub("/Cargo.toml$"; "") | ltrimstr($root + "/")),
        ([.dependencies[].name] | join(","))
      ]
    | @tsv
  ' <<< "$metadata"
)

for package in "${package_names[@]}"; do
  IFS=',' read -r -a deps <<< "${deps_by_package[$package]}"
  for dep in "${deps[@]}"; do
    [ -n "$dep" ] || continue
    if [ -n "${package_exists[$dep]+x}" ]; then
      if [ -n "${reverse_deps[$dep]+x}" ]; then
        reverse_deps["$dep"]+=",${package}"
      else
        reverse_deps["$dep"]=$package
      fi
    fi
  done
done

add_reason() {
  reasons+=("$1")
}

normalise_path() {
  local path=$1
  path=${path//$'\r'/}
  path=${path//\\//}
  while [[ "$path" == ./* ]]; do
    path=${path#./}
  done
  printf '%s\n' "$path"
}

is_doc_path() {
  local path=$1
  [[ "$path" == *.md || "$path" == docs/* || "$path" == README.md || "$path" == CONTRIBUTING.md || "$path" == AGENTS.md ]]
}

add_direct_package() {
  direct_packages["$1"]=1
}

all_packages=false
bench_all=false

classify_path() {
  local path=$1
  [ -n "$path" ] || return 0

  case "$path" in
    Cargo.lock|Cargo.toml|.github/workflows/rust-tests.yml|.cargo/*)
      all_packages=true
      add_reason "$path: workspace-wide Rust input"
      return 0
      ;;
    scripts/discover-benches.py)
      bench_all=true
      add_reason "$path: benchmark discovery changed"
      return 0
      ;;
    scripts/check-default-cli-contracts.sh)
      add_direct_package "$CLI_PACKAGE"
      add_reason "$path: CLI executable contract changed"
      return 0
      ;;
    scripts/fuse-bench-compare.py|scripts/tests/*)
      add_reason "$path: script-only Rust-lane check"
      return 0
      ;;
  esac

  if is_doc_path "$path"; then
    add_direct_package "$DOC_PACKAGE"
    add_reason "$path: docs are validated by heddle-cli doctor tests"
    return 0
  fi

  if [[ "$path" == crates/*/* ]]; then
    local rest=${path#crates/}
    local crate_name=${rest%%/*}
    local crate_dir="crates/$crate_name"
    local package=${package_by_dir[$crate_dir]:-}
    if [ -z "$package" ]; then
      all_packages=true
      add_reason "$path: unknown crate directory"
    else
      add_direct_package "$package"
      add_reason "$path: changed package $package"
    fi
    return 0
  fi

  all_packages=true
  add_reason "$path: unknown build-relevant path"
}

if [ "$select_all" = "true" ]; then
  all_packages=true
  bench_all=true
  add_reason "explicit full-workspace selection"
else
  path_count=0
  while IFS= read -r raw_path || [ -n "$raw_path" ]; do
    path=$(normalise_path "$raw_path")
    [ -n "$path" ] || continue
    path_count=$((path_count + 1))
    classify_path "$path"
  done < "$changed_paths"

  if [ "$path_count" -eq 0 ]; then
    all_packages=true
    bench_all=true
    add_reason "empty changed-path list; fail-closed full workspace"
  fi
fi

if [ "$all_packages" = "true" ]; then
  for package in "${package_names[@]}"; do
    selected_packages["$package"]=1
  done
else
  pending=()
  for package in "${!direct_packages[@]}"; do
    selected_packages["$package"]=1
    pending+=("$package")
  done

  while [ "${#pending[@]}" -gt 0 ]; do
    package=${pending[0]}
    pending=("${pending[@]:1}")
    IFS=',' read -r -a dependents <<< "${reverse_deps[$package]:-}"
    for dependent in "${dependents[@]}"; do
      [ -n "$dependent" ] || continue
      if [ -z "${selected_packages[$dependent]+x}" ]; then
        selected_packages["$dependent"]=1
        pending+=("$dependent")
      fi
    done
  done
fi

selected_names=()
for package in "${package_names[@]}"; do
  if [ -n "${selected_packages[$package]+x}" ]; then
    selected_names+=("$package")
  fi
done

if [ "$all_packages" = "true" ]; then
  cargo_package_args=--workspace
else
  cargo_package_args=
  for package in "${selected_names[@]}"; do
    if [ -n "$cargo_package_args" ]; then
      cargo_package_args+=" "
    fi
    cargo_package_args+="-p $package"
  done
fi

if [ "$bench_all" = "true" ] || [ "$all_packages" = "true" ]; then
  bench_all_output=true
else
  bench_all_output=false
fi

if [ "${#selected_names[@]}" -eq 0 ]; then
  skip_cargo=true
else
  skip_cargo=false
fi

join_by() {
  local delimiter=$1
  shift
  local first=true
  for item in "$@"; do
    if [ "$first" = "true" ]; then
      first=false
    else
      printf '%s' "$delimiter"
    fi
    printf '%s' "$item"
  done
}

package_names_csv=$(join_by "," "${selected_names[@]}")
reason=$(join_by "; " "${reasons[@]}")
reason=${reason//$'\n'/ }

outputs=(
  "all_packages=$all_packages"
  "skip_cargo=$skip_cargo"
  "bench_all=$bench_all_output"
  "package_names_csv=$package_names_csv"
  "cargo_package_args=$cargo_package_args"
  "reason=$reason"
)

if [ -n "$github_output" ]; then
  printf '%s\n' "${outputs[@]}" >> "$github_output"
else
  printf '%s\n' "${outputs[@]}"
fi

echo "Affected Rust package selection:"
for output in "${outputs[@]}"; do
  echo "  ${output%%=*}: ${output#*=}"
done
