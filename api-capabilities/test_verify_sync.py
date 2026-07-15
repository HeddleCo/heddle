#!/usr/bin/env python3
"""Tests for immutable API capability provenance."""

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from verify_sync import verify


class VerifySyncTests(unittest.TestCase):
    def test_mismatched_api_snapshot_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            declaration = b'{"consumer":"heddle"}\n'
            (root / "heddle.json").write_bytes(declaration)
            (root / "provenance.json").write_text(json.dumps({
                "consumer": "heddle",
                "api_repository": "HeddleCo/api",
                "api_revision": "a" * 40,
                "api_snapshot": "capabilities/declarations/heddle.json",
                "local_declaration": "heddle.json",
                "sha256": hashlib.sha256(declaration).hexdigest(),
            }))
            with self.assertRaisesRegex(SystemExit, "snapshot differs"):
                verify(root, b"stale")


if __name__ == "__main__":
    unittest.main()
