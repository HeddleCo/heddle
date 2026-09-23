#!/usr/bin/env python3
"""Generate fixed source-tree and binary delta corpora for delta_profile."""

import random
import subprocess
import sys
from pathlib import Path


def write_pair(directory: Path, name: str, base: bytes, target: bytes) -> None:
    (directory / f"{name}.base").write_bytes(base)
    (directory / f"{name}.target").write_bytes(target)
    print(f"{name}: base={len(base)} target={len(target)}")


def main() -> None:
    directory = Path(sys.argv[1])
    directory.mkdir(parents=True, exist_ok=True)
    repo = Path(__file__).resolve().parents[3]

    paths = subprocess.check_output(["git", "ls-files", "crates"], cwd=repo, text=True)
    source = bytearray()
    for name in sorted(paths.splitlines()):
        if not name.endswith(".rs") or name.startswith(("crates/format/", "crates/pack/")):
            continue
        content = (repo / name).read_bytes()
        source.extend(f"\n// {name}\n".encode())
        source.extend(content)
        if len(source) >= 2 * 1024 * 1024:
            break
    source = bytes(source)
    edited = bytearray(source)
    for offset in range(16 * 1024, len(edited), 64 * 1024):
        edited[offset : offset + 80] = b"// edited declaration\n" * 4
    edited[8000:8000] = b"// inserted source file\n" * 80
    del edited[600_000:601_000]
    write_pair(directory, "source_tree", source, bytes(edited))

    rng = random.Random(1817)
    binary = rng.randbytes(8 * 1024 * 1024)
    changed = bytearray(binary)
    for offset in range(32 * 1024, len(changed), 1024 * 1024):
        changed[offset : offset + 4096] = rng.randbytes(4096)
    changed[99_999:99_999] = rng.randbytes(2048)
    del changed[6_000_000:6_002_048]
    write_pair(directory, "binary_edits", binary, bytes(changed))

    unrelated = random.Random(2270).randbytes(2 * 1024 * 1024)
    write_pair(directory, "binary_unrelated", binary, unrelated)


if __name__ == "__main__":
    main()
