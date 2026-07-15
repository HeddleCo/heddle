#!/usr/bin/env python3
"""Verify this declaration's hash and immutable API provenance."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import urllib.error
import urllib.request
from pathlib import Path


ROOT = Path(__file__).resolve().parent


def verify(root: Path, snapshot: bytes | None = None) -> None:
    provenance = json.loads((root / "provenance.json").read_text())
    revision = provenance["api_revision"]
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise SystemExit("api_revision must be an immutable 40-character commit")
    declaration = root / provenance["local_declaration"]
    local = declaration.read_bytes()
    digest = hashlib.sha256(local).hexdigest()
    if digest != provenance["sha256"]:
        raise SystemExit("local declaration hash differs from provenance")

    if provenance["api_repository"] != "HeddleCo/api":
        raise SystemExit("unexpected API provenance repository")
    expected_snapshot = f"capabilities/declarations/{provenance['consumer']}.json"
    if provenance["api_snapshot"] != expected_snapshot:
        raise SystemExit("unexpected API snapshot path")
    if snapshot is None:
        url = (
            f"https://raw.githubusercontent.com/{provenance['api_repository']}/"
            f"{revision}/{provenance['api_snapshot']}"
        )
        try:
            with urllib.request.urlopen(url, timeout=30) as response:
                snapshot = response.read()
        except (OSError, urllib.error.URLError) as error:
            raise SystemExit(f"could not retrieve immutable API snapshot: {error}") from error
    if snapshot != local or hashlib.sha256(snapshot).hexdigest() != provenance["sha256"]:
        raise SystemExit("immutable API snapshot differs from consumer declaration")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--snapshot", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    verify(ROOT, args.snapshot.read_bytes() if args.snapshot else None)


if __name__ == "__main__":
    main()
