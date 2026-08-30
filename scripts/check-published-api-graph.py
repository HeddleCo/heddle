#!/usr/bin/env python3
"""Check latest published Heddle crates resolve one heddle-api major/minor."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import re
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from functools import total_ordering
from pathlib import Path

DEFAULT_WORKFLOW = ".github/workflows/publish-crates.yml"
DEFAULT_INDEX_URL = "https://index.crates.io"
USER_AGENT = "heddle-published-graph-check/1.0 (https://github.com/HeddleCo/heddle)"


class CheckError(Exception):
    """An index, configuration, or resolution error that must fail the check."""


def publishable_crates(path: Path) -> list[str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise CheckError(f"could not read {path}: {error}") from error

    crates: list[str] = []
    in_list = False
    for line in lines:
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
        raise CheckError(f"could not parse PUBLISHABLE_CRATES from {path}")
    if len(crates) != len(set(crates)):
        raise CheckError(f"PUBLISHABLE_CRATES contains duplicates in {path}")
    return crates


def sparse_index_path(crate: str) -> str:
    name = crate.lower()
    if len(name) == 1:
        return f"1/{name}"
    if len(name) == 2:
        return f"2/{name}"
    if len(name) == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[:2]}/{name[2:4]}/{name}"


@total_ordering
@dataclass(frozen=True)
class Version:
    major: int
    minor: int
    patch: int
    prerelease: tuple[str, ...] = ()

    _PATTERN = re.compile(
        r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
        r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
        r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
    )

    @classmethod
    def parse(cls, value: object, source: str) -> Version:
        if not isinstance(value, str):
            raise CheckError(f"{source} has a non-string version")
        match = cls._PATTERN.fullmatch(value)
        if match is None:
            raise CheckError(f"{source} has invalid semantic version {value!r}")
        prerelease = tuple(match.group(4).split(".")) if match.group(4) else ()
        return cls(*(int(match.group(i)) for i in range(1, 4)), prerelease)

    def __str__(self) -> str:
        value = f"{self.major}.{self.minor}.{self.patch}"
        if self.prerelease:
            value += "-" + ".".join(self.prerelease)
        return value

    def __lt__(self, other: object) -> bool:
        if not isinstance(other, Version):
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


class Index:
    def __init__(self, base_url: str, directory: Path | None) -> None:
        self.base_url = base_url.rstrip("/")
        self.directory = directory

    def records(self, crate: str) -> list[dict] | None:
        relative = sparse_index_path(crate)
        if self.directory is not None:
            path = self.directory / relative
            if not path.exists():
                return None
            try:
                contents = path.read_text(encoding="utf-8")
            except OSError as error:
                raise CheckError(
                    f"could not read sparse index entry {path}: {error}"
                ) from error
        else:
            contents = self._download(crate, relative)
            if contents is None:
                return None

        records: list[dict] = []
        for line_number, line in enumerate(contents.splitlines(), start=1):
            if not line.strip():
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError as error:
                raise CheckError(
                    f"invalid JSON in sparse index entry for {crate}, line {line_number}: {error}"
                ) from error
            if not isinstance(record, dict):
                raise CheckError(
                    f"non-object sparse index record for {crate}, line {line_number}"
                )
            records.append(record)
        if not records:
            raise CheckError(f"sparse index entry for {crate} is empty")
        return records

    def _download(self, crate: str, relative: str) -> str | None:
        url = f"{self.base_url}/{relative}"
        request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
        for attempt in range(1, 4):
            try:
                with urllib.request.urlopen(request, timeout=30) as response:
                    return response.read().decode("utf-8")
            except urllib.error.HTTPError as error:
                if error.code == 404:
                    return None
                last_error: Exception = error
            except (urllib.error.URLError, TimeoutError, UnicodeDecodeError) as error:
                last_error = error
            if attempt < 3:
                time.sleep(attempt)
        raise CheckError(
            f"could not fetch sparse index entry for {crate}: {last_error}"
        )


@dataclass(frozen=True)
class LatestCrate:
    name: str
    version: Version
    record: dict


@dataclass(frozen=True)
class ApiResolution:
    crate: str
    crate_version: Version
    requirement: str
    compatible_versions: tuple[Version, ...]

    def latest_on(self, line: tuple[int, int] | None = None) -> Version:
        if line is None:
            return self.compatible_versions[0]
        return next(
            version
            for version in self.compatible_versions
            if (version.major, version.minor) == line
        )


def latest_stable(crate: str, records: list[dict]) -> LatestCrate:
    candidates: list[tuple[Version, dict]] = []
    for line_number, record in enumerate(records, start=1):
        version = Version.parse(record.get("vers"), f"{crate} index line {line_number}")
        if record.get("yanked") is True or version.prerelease:
            continue
        candidates.append((version, record))
    if not candidates:
        raise CheckError(
            f"{crate} has no non-yanked stable version in the sparse index"
        )
    version, record = max(candidates, key=lambda candidate: candidate[0])
    return LatestCrate(crate, version, record)


def parse_partial(value: str, requirement: str) -> tuple[list[int], bool]:
    if "-" in value or "+" in value:
        raise CheckError(
            f"pre-release/build metadata is unsupported in heddle-api requirement {requirement!r}"
        )
    parts = value.split(".")
    if len(parts) > 3:
        raise CheckError(f"invalid heddle-api requirement {requirement!r}")
    numbers: list[int] = []
    wildcard = False
    for index, part in enumerate(parts):
        if part in ("*", "x", "X"):
            wildcard = True
            if index != len(parts) - 1:
                raise CheckError(
                    f"invalid heddle-api wildcard requirement {requirement!r}"
                )
            break
        if not part.isdigit() or (len(part) > 1 and part.startswith("0")):
            raise CheckError(f"invalid heddle-api requirement {requirement!r}")
        numbers.append(int(part))
    return numbers, wildcard


def padded(numbers: list[int]) -> Version:
    values = [*numbers, 0, 0, 0]
    return Version(values[0], values[1], values[2])


def caret_upper(numbers: list[int]) -> Version:
    values = [*numbers, 0, 0, 0]
    if not numbers or values[0] != 0 or len(numbers) == 1:
        return Version(values[0] + 1, 0, 0)
    if values[1] != 0 or len(numbers) == 2:
        return Version(0, values[1] + 1, 0)
    return Version(0, 0, values[2] + 1)


def comparator_matches(comparator: str, version: Version, requirement: str) -> bool:
    match = re.fullmatch(r"(\^|~|>=|<=|>|<|=)?\s*(.+)", comparator)
    if match is None:
        raise CheckError(f"invalid heddle-api requirement {requirement!r}")
    operator = match.group(1) or "^"
    value = match.group(2).strip()
    numbers, wildcard = parse_partial(value, requirement)

    if wildcard:
        return (version.major, version.minor, version.patch)[: len(numbers)] == tuple(
            numbers
        )
    if not numbers:
        raise CheckError(f"invalid heddle-api requirement {requirement!r}")

    lower = padded(numbers)
    if operator == ">=":
        return version >= lower
    if operator == ">":
        return version > lower
    if operator == "<=":
        return version <= lower
    if operator == "<":
        return version < lower
    if operator == "=":
        if len(numbers) < 3:
            return (version.major, version.minor, version.patch)[
                : len(numbers)
            ] == tuple(numbers)
        return version == lower
    if operator == "~":
        if len(numbers) == 1:
            upper = Version(numbers[0] + 1, 0, 0)
        else:
            upper = Version(numbers[0], numbers[1] + 1, 0)
        return lower <= version < upper
    return lower <= version < caret_upper(numbers)


def requirement_matches(requirement: str, version: Version) -> bool:
    if requirement.strip() == "*":
        return True
    comparators = [part.strip() for part in requirement.split(",") if part.strip()]
    if not comparators:
        raise CheckError("empty heddle-api version requirement")
    return all(
        comparator_matches(comparator, version, requirement)
        for comparator in comparators
    )


def api_requirements(latest: LatestCrate) -> list[str]:
    deps = latest.record.get("deps")
    if not isinstance(deps, list):
        raise CheckError(f"{latest.name}@{latest.version} has no dependency list")
    requirements: list[str] = []
    for dependency in deps:
        if not isinstance(dependency, dict):
            raise CheckError(
                f"{latest.name}@{latest.version} has a malformed dependency"
            )
        actual_name = dependency.get("package") or dependency.get("name")
        if actual_name != "heddle-api" or dependency.get("kind") == "dev":
            continue
        requirement = dependency.get("req")
        if not isinstance(requirement, str):
            raise CheckError(
                f"{latest.name}@{latest.version} has a non-string heddle-api requirement"
            )
        requirements.append(requirement)
    return requirements


def run(workflow: Path, index: Index) -> int:
    crates = publishable_crates(workflow)
    print(f"ok: read {len(crates)} publishable crates from {workflow}")

    all_names = [*crates, "heddle-api"]
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
        futures = {name: executor.submit(index.records, name) for name in all_names}
        records_by_name = {name: future.result() for name, future in futures.items()}

    api_records = records_by_name["heddle-api"]
    if api_records is None:
        raise CheckError("heddle-api is not present in the sparse index")
    api_version_set: set[Version] = set()
    for line_number, record in enumerate(api_records, start=1):
        version = Version.parse(
            record.get("vers"), f"heddle-api index line {line_number}"
        )
        if record.get("yanked") is not True and not version.prerelease:
            api_version_set.add(version)
    if not api_version_set:
        raise CheckError(
            "heddle-api has no non-yanked stable version in the sparse index"
        )
    api_versions = sorted(api_version_set, reverse=True)

    resolutions: list[ApiResolution] = []
    unpublished: list[str] = []
    published_count = 0
    for crate in crates:
        records = records_by_name[crate]
        if records is None:
            unpublished.append(crate)
            continue
        latest = latest_stable(crate, records)
        published_count += 1
        for requirement in api_requirements(latest):
            compatible = tuple(
                version
                for version in api_versions
                if requirement_matches(requirement, version)
            )
            if not compatible:
                raise CheckError(
                    f"{crate}@{latest.version} requires heddle-api {requirement}, "
                    "but no published non-yanked stable version satisfies it"
                )
            resolutions.append(
                ApiResolution(crate, latest.version, requirement, compatible)
            )

    if unpublished:
        print(
            "note: not published yet (excluded from the published set): "
            + ", ".join(unpublished)
        )
    print(
        f"ok: inspected the latest non-yanked stable release of "
        f"{published_count}/{len(crates)} publishable crates"
    )
    sys.stdout.flush()
    if not resolutions:
        raise CheckError(
            "latest published crate set contains no heddle-api requirements"
        )

    compatible_lines = [
        {(version.major, version.minor) for version in item.compatible_versions}
        for item in resolutions
    ]
    common_lines = set.intersection(*compatible_lines)

    if not common_lines:
        lines: dict[tuple[int, int], list[ApiResolution]] = {}
        for resolution in resolutions:
            latest = resolution.latest_on()
            lines.setdefault((latest.major, latest.minor), []).append(resolution)
        print(
            f"error: published heddle-api graph is split across {len(lines)} major/minor lines",
            file=sys.stderr,
        )
        for line, entries in sorted(lines.items()):
            print(f"  heddle-api {line[0]}.{line[1]}:", file=sys.stderr)
            for entry in entries:
                print(
                    f"    - {entry.crate}@{entry.crate_version} requires "
                    f"{entry.requirement} -> {entry.latest_on()}",
                    file=sys.stderr,
                )
        return 1

    line = max(common_lines)
    resolved_versions = ", ".join(
        str(version)
        for version in sorted({item.latest_on(line) for item in resolutions})
    )
    print(
        f"ok: {len(resolutions)} requirements from latest published crates converge "
        f"on heddle-api {line[0]}.{line[1]} (resolved: {resolved_versions})"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workflow", type=Path, default=Path(DEFAULT_WORKFLOW))
    parser.add_argument("--index-url", default=DEFAULT_INDEX_URL)
    parser.add_argument(
        "--index-dir",
        type=Path,
        help="read a local sparse index fixture instead of crates.io",
    )
    args = parser.parse_args()
    try:
        return run(args.workflow, Index(args.index_url, args.index_dir))
    except CheckError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
