#!/usr/bin/env python3
"""Prove the hosted Git reconstruction surface's normal graph is engine-free."""
import subprocess
import sys
from pathlib import Path

FORBIDDEN = {
    "heddle-repo", "sley-worktree", "sley-hooks", "sley-sequencer", "ureq",
}


def main() -> int:
    command = [
        "cargo", "tree", "--color", "never", "--locked", "-p", "heddle-git-serialize",
        "-e", "normal", "--prefix", "none", "--format", "{p}",
    ]
    result = subprocess.run(
        command, cwd=Path(__file__).resolve().parent.parent,
        capture_output=True, text=True, timeout=120, check=False,
    )
    if result.returncode:
        sys.stderr.write(result.stderr)
        return result.returncode
    names = {line.split()[0] for line in result.stdout.splitlines() if line.strip()}
    if "heddle-git-serialize" not in names:
        print("error: cargo tree did not report the reconstruction crate", file=sys.stderr)
        return 1
    forbidden = sorted(names & FORBIDDEN)
    if forbidden:
        print(f"error: forbidden normal dependencies: {', '.join(forbidden)}", file=sys.stderr)
        sys.stderr.write(result.stdout)
        return 1
    print("ok: cargo tree --locked -p heddle-git-serialize -e normal")
    print(f"ok: {len(names)} normal crate names; absent: {', '.join(sorted(FORBIDDEN))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
