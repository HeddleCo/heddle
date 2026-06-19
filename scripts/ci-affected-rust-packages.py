#!/usr/bin/env python3
"""Select workspace packages affected by a changed-path list.

The CI Rust lane uses this to avoid rebuilding and testing the whole workspace
when a PR or push only touches a leaf crate. Package selection is intentionally
fail-closed: workspace-wide inputs or unknown build-relevant paths select the
whole workspace. Crate paths select the owning crate plus every workspace crate
that depends on it, including dev-dependencies, because `cargo test` and
`cargo clippy --all-targets` compile test targets too.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import Iterable


WORKSPACE_WIDE_PATHS = {
    "Cargo.lock",
    "Cargo.toml",
    ".github/workflows/rust-tests.yml",
}

DOC_PACKAGE = "heddle-cli"
CLI_PACKAGE = "heddle-cli"


def run_metadata() -> dict:
    raw = subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--no-deps"],
        text=True,
    )
    return json.loads(raw)


def normalise_path(raw: str) -> str:
    return raw.strip().replace("\\", "/").removeprefix("./")


def workspace_packages(metadata: dict) -> tuple[list[str], dict[str, dict], dict[str, str]]:
    workspace_ids = set(metadata["workspace_members"])
    packages = [pkg for pkg in metadata["packages"] if pkg["id"] in workspace_ids]
    order = {pkg_id: index for index, pkg_id in enumerate(metadata["workspace_members"])}
    packages.sort(key=lambda pkg: order[pkg["id"]])

    by_name = {pkg["name"]: pkg for pkg in packages}
    by_dir = {
        str(Path(pkg["manifest_path"]).parent.relative_to(metadata["workspace_root"])).replace(
            "\\", "/"
        ): pkg["name"]
        for pkg in packages
    }
    names = [pkg["name"] for pkg in packages]
    return names, by_name, by_dir


def reverse_dependencies(names: Iterable[str], by_name: dict[str, dict]) -> dict[str, set[str]]:
    workspace_names = set(names)
    reverse: dict[str, set[str]] = {name: set() for name in workspace_names}
    for package in by_name.values():
        for dep in package.get("dependencies", []):
            dep_name = dep["name"]
            if dep_name in workspace_names:
                reverse[dep_name].add(package["name"])
    return reverse


def closure(seed: set[str], reverse: dict[str, set[str]]) -> set[str]:
    selected = set(seed)
    pending = list(seed)
    while pending:
        package = pending.pop()
        for dependent in reverse.get(package, set()):
            if dependent not in selected:
                selected.add(dependent)
                pending.append(dependent)
    return selected


def is_doc_path(path: str) -> bool:
    return (
        path.endswith(".md")
        or path.startswith("docs/")
        or path in {"README.md", "CONTRIBUTING.md", "AGENTS.md"}
    )


def classify_paths(paths: list[str], by_dir: dict[str, str]) -> tuple[bool, set[str], bool, list[str]]:
    """Return (all_packages, direct_packages, bench_all, reasons)."""
    all_packages = False
    bench_all = False
    direct: set[str] = set()
    reasons: list[str] = []

    for path in paths:
        if not path:
            continue

        if path in WORKSPACE_WIDE_PATHS or path.startswith(".cargo/"):
            all_packages = True
            reasons.append(f"{path}: workspace-wide Rust input")
            continue

        if path == "scripts/discover-benches.py":
            bench_all = True
            reasons.append(f"{path}: benchmark discovery changed")
            continue

        if path == "scripts/check-default-install-ships-worker.sh":
            direct.add(CLI_PACKAGE)
            reasons.append(f"{path}: CLI install contract changed")
            continue

        if path == "scripts/fuse-bench-compare.py" or path.startswith("scripts/tests/"):
            reasons.append(f"{path}: script-only Rust-lane check")
            continue

        if is_doc_path(path):
            direct.add(DOC_PACKAGE)
            reasons.append(f"{path}: docs are validated by heddle-cli doctor tests")
            continue

        parts = path.split("/")
        if len(parts) >= 2 and parts[0] == "crates":
            crate_dir = "/".join(parts[:2])
            package = by_dir.get(crate_dir)
            if package is None:
                all_packages = True
                reasons.append(f"{path}: unknown crate directory")
            else:
                direct.add(package)
                reasons.append(f"{path}: changed package {package}")
            continue

        all_packages = True
        reasons.append(f"{path}: unknown build-relevant path")

    return all_packages, direct, bench_all, reasons


def write_output(path: str | None, values: dict[str, str]) -> None:
    lines = [f"{key}={value}" for key, value in values.items()]
    text = "\n".join(lines) + "\n"
    if path:
        with open(path, "a", encoding="utf-8") as out:
            out.write(text)
    else:
        sys.stdout.write(text)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--changed-paths", type=Path)
    parser.add_argument("--all", action="store_true", help="select the whole workspace")
    parser.add_argument("--metadata-json", type=Path, help="test seam: read cargo metadata JSON")
    parser.add_argument("--github-output", help="append outputs to this file")
    args = parser.parse_args()

    if not args.all and not args.changed_paths:
        parser.error("either --all or --changed-paths is required")

    metadata = json.loads(args.metadata_json.read_text()) if args.metadata_json else run_metadata()
    names, by_name, by_dir = workspace_packages(metadata)
    reverse = reverse_dependencies(names, by_name)

    if args.all:
        selected = set(names)
        all_packages = True
        bench_all = True
        reasons = ["explicit full-workspace selection"]
    else:
        paths = [normalise_path(line) for line in args.changed_paths.read_text().splitlines()]
        paths = [path for path in paths if path]
        if not paths:
            selected = set(names)
            all_packages = True
            bench_all = True
            reasons = ["empty changed-path list; fail-closed full workspace"]
        else:
            all_packages, direct, bench_all, reasons = classify_paths(paths, by_dir)
            selected = set(names) if all_packages else closure(direct, reverse)

    selected_names = [name for name in names if name in selected]
    if all_packages:
        cargo_package_args = "--workspace"
    else:
        cargo_package_args = " ".join(f"-p {name}" for name in selected_names)

    outputs = {
        "all_packages": "true" if all_packages else "false",
        "skip_cargo": "true" if not selected_names else "false",
        "bench_all": "true" if bench_all or all_packages else "false",
        "package_names_csv": ",".join(selected_names),
        "cargo_package_args": cargo_package_args,
        "reason": "; ".join(reasons).replace("\n", " "),
    }
    write_output(args.github_output, outputs)

    print("Affected Rust package selection:")
    for key, value in outputs.items():
        print(f"  {key}: {value}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
