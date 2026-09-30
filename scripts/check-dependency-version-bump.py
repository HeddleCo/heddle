#!/usr/bin/env python3
"""Require version bumps when publishable dependency contracts change."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass
from functools import total_ordering

import tomllib

PUBLISH_WORKFLOW = ".github/workflows/publish-crates.yml"


class CheckError(Exception):
    """A configuration or repository error that must fail the gate."""


def git(*args: str) -> str:
    result = subprocess.run(
        ["git", *args],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise CheckError(f"git {' '.join(args)} failed: {detail}")
    return result.stdout


def read_at(revision: str, path: str) -> str | None:
    result = subprocess.run(
        ["git", "show", f"{revision}:{path}"],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode == 0:
        return result.stdout
    if result.returncode == 128 and "does not exist" in result.stderr:
        return None
    detail = result.stderr.strip() or result.stdout.strip()
    raise CheckError(f"git show {revision}:{path} failed: {detail}")


def parse_toml(contents: str, source: str) -> dict:
    try:
        return tomllib.loads(contents)
    except tomllib.TOMLDecodeError as error:
        raise CheckError(f"could not parse {source}: {error}") from error


def publishable_crates(contents: str, source: str) -> list[str]:
    crates: list[str] = []
    in_list = False
    for line in contents.splitlines():
        if line == "  PUBLISHABLE_CRATES: |":
            in_list = True
            continue
        if in_list:
            if line.startswith("    "):
                name = line.strip()
                if name:
                    crates.append(name)
                continue
            break
    if not crates:
        raise CheckError(f"could not parse PUBLISHABLE_CRATES from {source}")
    if len(crates) != len(set(crates)):
        raise CheckError(f"PUBLISHABLE_CRATES contains duplicates in {source}")
    return crates


def manifests_at(revision: str) -> dict[str, tuple[str, dict]]:
    paths = git("ls-tree", "-r", "--name-only", revision, "--", "crates")
    manifests: dict[str, tuple[str, dict]] = {}
    for path in paths.splitlines():
        if not path.endswith("/Cargo.toml"):
            continue
        contents = read_at(revision, path)
        if contents is None:
            continue
        parsed = parse_toml(contents, f"{revision}:{path}")
        name = parsed.get("package", {}).get("name")
        if not isinstance(name, str):
            continue
        if name in manifests:
            raise CheckError(f"duplicate package name {name!r} at {revision}")
        manifests[name] = (path, parsed)
    return manifests


@total_ordering
@dataclass(frozen=True)
class SemVer:
    major: int
    minor: int
    patch: int
    prerelease: tuple[str, ...]

    _PATTERN = re.compile(
        r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
        r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
        r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
    )

    @classmethod
    def parse(cls, value: object, source: str) -> SemVer:
        if not isinstance(value, str):
            raise CheckError(f"{source} must be a semantic version string")
        match = cls._PATTERN.fullmatch(value)
        if match is None:
            raise CheckError(f"{source} is not a valid semantic version: {value!r}")
        prerelease = tuple(match.group(4).split(".")) if match.group(4) else ()
        return cls(*(int(match.group(i)) for i in range(1, 4)), prerelease)

    def __lt__(self, other: object) -> bool:
        if not isinstance(other, SemVer):
            return NotImplemented
        left_core = (self.major, self.minor, self.patch)
        right_core = (other.major, other.minor, other.patch)
        if left_core != right_core:
            return left_core < right_core
        if not self.prerelease:
            return False
        if not other.prerelease:
            return True
        for left, right in zip(self.prerelease, other.prerelease):
            if left == right:
                continue
            if left.isdigit() and right.isdigit():
                return int(left) < int(right)
            if left.isdigit() != right.isdigit():
                return left.isdigit()
            return left < right
        return len(self.prerelease) < len(other.prerelease)


def revision_state(
    revision: str,
) -> tuple[dict, list[str], dict[str, tuple[str, dict]]]:
    root_contents = read_at(revision, "Cargo.toml")
    if root_contents is None:
        raise CheckError(f"Cargo.toml does not exist at {revision}")
    workflow_contents = read_at(revision, PUBLISH_WORKFLOW)
    if workflow_contents is None:
        raise CheckError(f"{PUBLISH_WORKFLOW} does not exist at {revision}")
    return (
        parse_toml(root_contents, f"{revision}:Cargo.toml"),
        publishable_crates(workflow_contents, f"{revision}:{PUBLISH_WORKFLOW}"),
        manifests_at(revision),
    )


def dependency_contract(manifest: dict, workspace: dict) -> dict:
    """Resolve inherited requirements in every packaged dependency table."""
    contract = {}
    tables = [("", manifest), *manifest.get("target", {}).items()]
    for target, table in tables:
        for kind in ("dependencies", "build-dependencies", "dev-dependencies"):
            dependencies = {}
            for name, value in table.get(kind, {}).items():
                dependency = {"version": value} if isinstance(value, str) else dict(value)
                if dependency.pop("workspace", False):
                    inherited = workspace.get("dependencies", {}).get(name)
                    if inherited is None:
                        raise CheckError(f"missing workspace dependency {name!r}")
                    resolved = (
                        {"version": inherited} if isinstance(inherited, str) else dict(inherited)
                    )
                    features = set(resolved.get("features", []))
                    features.update(dependency.pop("features", []))
                    resolved.update(dependency)
                    if features:
                        resolved["features"] = sorted(features)
                    dependency = resolved
                # Paths select local sources, but are stripped from published
                # manifests; moving a checkout must not force a release.
                dependency.pop("path", None)
                if "features" in dependency:
                    dependency["features"] = sorted(set(dependency["features"]))
                dependencies[name] = dependency
            if dependencies:
                contract[(target, kind)] = dependencies
    return contract


@dataclass(frozen=True)
class RequiredBump:
    source: str
    base: str
    head: str


def dependency_changes(base: str, head: str) -> tuple[list[str], list[RequiredBump]]:
    base_root, base_publishable, base_manifests = revision_state(base)
    head_root, head_publishable, head_manifests = revision_state(head)

    changes: list[str] = []
    bumps: list[RequiredBump] = []
    base_workspace = base_root.get("workspace", {})
    head_workspace = head_root.get("workspace", {})
    workspace_changed = base_workspace.get("dependencies", {}) != head_workspace.get(
        "dependencies", {}
    )
    if workspace_changed:
        changes.append("Cargo.toml [workspace.dependencies]")

    names = dict.fromkeys([*base_publishable, *head_publishable])
    for name in names:
        base_entry = base_manifests.get(name)
        head_entry = head_manifests.get(name)
        # A newly added/removed crate has no existing release to compare.
        if base_entry is None or head_entry is None:
            continue
        path, manifest = head_entry
        if dependency_contract(base_entry[1], base_workspace) == dependency_contract(
            manifest, head_workspace
        ):
            continue
        changes.append(f"{path} [dependencies] (including build/dev/target; {name})")
        head_version = manifest.get("package", {}).get("version")
        if isinstance(head_version, dict) and head_version.get("workspace") is True:
            workspace_changed = True
        else:
            base_version = base_entry[1].get("package", {}).get("version")
            # If a crate stops inheriting the workspace version, compare its
            # new independent version with the version consumers had before.
            if isinstance(base_version, dict) and base_version.get("workspace") is True:
                base_version = base_workspace.get("package", {}).get("version")
            source = f"{path} [package] version ({name})"
            SemVer.parse(base_version, f"{base}:{source}")
            SemVer.parse(head_version, f"{head}:{source}")
            bumps.append(RequiredBump(source, str(base_version), str(head_version)))

    base_version = base_workspace.get("package", {}).get("version")
    head_version = head_workspace.get("package", {}).get("version")
    SemVer.parse(base_version, f"{base}:workspace.package.version")
    SemVer.parse(head_version, f"{head}:workspace.package.version")
    if workspace_changed:
        bumps.append(RequiredBump("workspace", str(base_version), str(head_version)))
    return changes, bumps


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", help="PR base commit")
    parser.add_argument("head", nargs="?", default="HEAD", help="PR head commit")
    args = parser.parse_args()

    try:
        changes, bumps = dependency_changes(args.base, args.head)
    except CheckError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    if not changes:
        print("ok: no publishable dependency contracts changed")
        return 0

    print("changed publishable dependency contracts:")
    for change in changes:
        print(f"  - {change}")
    sys.stdout.flush()

    failed = False
    for bump in bumps:
        if SemVer.parse(bump.head, bump.source) <= SemVer.parse(bump.base, bump.source):
            source = (
                "[workspace.package] version in Cargo.toml"
                if bump.source == "workspace" else bump.source
            )
            print(
                f"error: dependency changes require an increased {source} "
                f"(base {bump.base}, head {bump.head})",
                file=sys.stderr,
            )
            failed = True
        else:
            print(
                f"ok: {bump.source}{' version' if bump.source == 'workspace' else ''} increased "
                f"from {bump.base} to {bump.head}"
            )
    if failed:
        print(
            "note: release-plz auto-bump wiring is the fuller fix; until then "
            "this bump is manual",
            file=sys.stderr,
        )
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
