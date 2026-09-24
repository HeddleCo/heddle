#!/usr/bin/env python3
"""Build the publishable crate set as a fresh crates.io consumer would.

Published versions come from crates.io. New versions come from cargo's .crate
archives, whose manifests have had workspace and path dependencies removed.
Run from the repository root with CARGO_TARGET_DIR set.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import urllib.error
import urllib.request

WORKFLOW = Path(".github/workflows/publish-crates.yml")
USER_AGENT = "heddle-consumer-build-check/1.0 (https://github.com/HeddleCo/heddle)"
CONSUMERS = (
    "heddle-repo",
    "heddle-biscuit-verifier",
    "heddle-format",
    "heddleco-capability-verifier",
)


def publishable_crates() -> list[str]:
    lines = WORKFLOW.read_text().splitlines()
    start = lines.index("  PUBLISHABLE_CRATES: |") + 1
    crates = []
    for line in lines[start:]:
        if not line.startswith("    "):
            break
        crates.append(line.strip())
    return crates


def sparse_index_path(crate: str) -> str:
    if len(crate) == 1:
        return f"1/{crate}"
    if len(crate) == 2:
        return f"2/{crate}"
    if len(crate) == 3:
        return f"3/{crate[0]}/{crate}"
    return f"{crate[:2]}/{crate[2:4]}/{crate}"


def published_versions(crate: str) -> set[str]:
    url = f"https://index.crates.io/{sparse_index_path(crate)}"
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return {
                record["vers"]
                for line in response
                if (record := json.loads(line)).get("yanked") is not True
            }
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return set()
        raise


def main() -> int:
    target = os.environ.get("CARGO_TARGET_DIR")
    if not target:
        print("error: set CARGO_TARGET_DIR before running this gate", file=sys.stderr)
        return 2

    manifests: dict[str, tuple[str, Path]] = {}
    workspace = tomllib.loads(Path("Cargo.toml").read_text())
    workspace_version = workspace["workspace"]["package"]["version"]
    for path in Path("crates").glob("*/Cargo.toml"):
        package = tomllib.loads(path.read_text())["package"]
        version = package.get("version", workspace_version)
        if isinstance(version, dict) and version.get("workspace") is True:
            version = workspace_version
        manifests[package["name"]] = (version, path)

    crates = publishable_crates()
    missing = set(crates) - manifests.keys()
    if missing:
        print(f"error: missing publishable manifests: {sorted(missing)}", file=sys.stderr)
        return 2

    new_crates = []
    for crate in crates:
        version, _ = manifests[crate]
        if version in published_versions(crate):
            print(f"registry: {crate}@{version}", flush=True)
        else:
            print(f"package: {crate}@{version}", flush=True)
            new_crates.append(crate)

    with tempfile.TemporaryDirectory(prefix="heddle-consumer-") as scratch:
        root = Path(scratch)
        patches = []
        package_configs = []
        for crate in new_crates:
            version, _ = manifests[crate]
            subprocess.run(
                [
                    "cargo", "package", "--allow-dirty", "--no-verify", "-p", crate,
                    *package_configs,
                ],
                check=True,
            )
            archive = Path(target) / "package" / f"{crate}-{version}.crate"
            with tarfile.open(archive) as package:
                package.extractall(root / "packages", filter="data")
            path = root / "packages" / f"{crate}-{version}"
            patches.append(f'{crate} = {{ path = "{path}" }}')
            package_configs.extend(
                ["--config", f"patch.crates-io.{crate}.path={json.dumps(str(path))}"]
            )

        deps = [f'{crate} = "={manifests[crate][0]}"' for crate in CONSUMERS]
        manifest = "\n".join(
            [
                '[package]',
                'name = "heddle-publish-consumer"',
                'version = "0.0.0"',
                'edition = "2024"',
                '',
                '[dependencies]',
                *deps,
                '',
                '[patch.crates-io]',
                *patches,
                '',
            ]
        )
        (root / "Cargo.toml").write_text(manifest)
        (root / "src").mkdir()
        (root / "src" / "main.rs").write_text("fn main() {}\n")
        print("building scratch consumer from registry and packaged sources", flush=True)
        subprocess.run(["cargo", "build", "--manifest-path", str(root / "Cargo.toml")], check=True)

    print("ok: published consumer graph builds")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"error: consumer build failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
