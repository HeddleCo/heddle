#!/usr/bin/env python3
"""Negative tests for Heddle capability evidence and provenance."""

from __future__ import annotations

import copy
import hashlib
import json
import tempfile
import unittest
import urllib.error
from pathlib import Path

from verify_sync import (
    API_REPOSITORY,
    API_REVISION,
    API_SNAPSHOT,
    CONSUMER,
    LOCAL_DECLARATION,
    sanitized_bytes,
    verify,
)


RPC = "heddle.api.v1alpha1.RepositoryService/GetCompare"


class VerifySyncTests(unittest.TestCase):
    def fixture(self) -> tuple[tempfile.TemporaryDirectory[str], Path, bytes]:
        temporary = tempfile.TemporaryDirectory()
        root = Path(temporary.name) / "api-capabilities"
        root.mkdir()
        source = Path(temporary.name) / "src" / "mapping.rs"
        source.parent.mkdir()
        source.write_text("async fn get_compare() { client.get_compare(); }\n")
        declaration = {
            "schema_version": 2,
            "consumer": CONSUMER,
            "source_repository": "HeddleCo/heddle",
            "rpc_mappings": [{
                "rpc": RPC,
                "layers": {
                    "client": {"status": "shipped", "evidence": [{"path": "src/mapping.rs", "contains": "get_compare"}], "follow_up": None},
                    "cli": {"status": "shipped", "evidence": [{"path": "src/mapping.rs", "contains": "client.get_compare("}], "follow_up": None},
                },
            }],
        }
        local = (json.dumps(declaration, indent=2) + "\n").encode()
        (root / LOCAL_DECLARATION).write_bytes(local)
        snapshot = sanitized_bytes(declaration, Path(temporary.name))
        provenance = {
            "schema_version": 2,
            "consumer": CONSUMER,
            "api_repository": API_REPOSITORY,
            "api_revision": API_REVISION,
            "api_snapshot": API_SNAPSHOT,
            "local_declaration": LOCAL_DECLARATION,
            "local_sha256": hashlib.sha256(local).hexdigest(),
            "sanitized_sha256": hashlib.sha256(snapshot).hexdigest(),
        }
        (root / "provenance.json").write_text(json.dumps(provenance))
        return temporary, root, snapshot

    def test_valid_fixture_matches_snapshot(self) -> None:
        temporary, root, snapshot = self.fixture()
        with temporary:
            verify(root, snapshot)

    def test_wrong_consumer_fails(self) -> None:
        temporary, root, snapshot = self.fixture()
        with temporary:
            provenance = json.loads((root / "provenance.json").read_text())
            provenance["consumer"] = "weft"
            (root / "provenance.json").write_text(json.dumps(provenance))
            with self.assertRaisesRegex(SystemExit, "identity, path, or revision"):
                verify(root, snapshot)

    def test_wrong_local_path_fails(self) -> None:
        temporary, root, snapshot = self.fixture()
        with temporary:
            provenance = json.loads((root / "provenance.json").read_text())
            provenance["local_declaration"] = "substitute.json"
            (root / "provenance.json").write_text(json.dumps(provenance))
            with self.assertRaisesRegex(SystemExit, "identity, path, or revision"):
                verify(root, snapshot)

    def test_invented_revision_fails_even_with_supplied_snapshot(self) -> None:
        temporary, root, snapshot = self.fixture()
        with temporary:
            provenance = json.loads((root / "provenance.json").read_text())
            provenance["api_revision"] = "a" * 40
            (root / "provenance.json").write_text(json.dumps(provenance))
            with self.assertRaisesRegex(SystemExit, "identity, path, or revision"):
                verify(root, snapshot)

    def test_invented_attestation_fails(self) -> None:
        temporary, root, snapshot = self.fixture()
        with temporary:
            provenance = json.loads((root / "provenance.json").read_text())
            provenance["sanitized_sha256"] = "0" * 64
            (root / "provenance.json").write_text(json.dumps(provenance))
            with self.assertRaisesRegex(SystemExit, "attestation"):
                verify(root, snapshot)

    def test_missing_cli_call_edge_fails(self) -> None:
        temporary, root, snapshot = self.fixture()
        with temporary:
            source = root.parent / "src" / "mapping.rs"
            source.write_text("fn get_compare() {}\n")
            with self.assertRaisesRegex(SystemExit, "structural client binding is missing"):
                verify(root, snapshot)

    def test_comment_only_name_does_not_count_as_client_binding(self) -> None:
        temporary, root, snapshot = self.fixture()
        with temporary:
            source = root.parent / "src" / "mapping.rs"
            source.write_text("// get_compare is planned\nclient.other_method();\n")
            with self.assertRaisesRegex(SystemExit, "structural client binding is missing"):
                verify(root, snapshot)

    def test_unrelated_call_name_does_not_bind_an_rpc(self) -> None:
        temporary, root, snapshot = self.fixture()
        with temporary:
            declaration_path = root / LOCAL_DECLARATION
            declaration = json.loads(declaration_path.read_text())
            declaration["rpc_mappings"][0]["layers"]["cli"]["evidence"][0]["contains"] = ".other_method("
            (root.parent / "src" / "mapping.rs").write_text(
                "async fn get_compare() { client.other_method(); }\n"
            )
            local = (json.dumps(declaration, indent=2) + "\n").encode()
            declaration_path.write_bytes(local)
            provenance = json.loads((root / "provenance.json").read_text())
            provenance["local_sha256"] = hashlib.sha256(local).hexdigest()
            (root / "provenance.json").write_text(json.dumps(provenance))
            with self.assertRaisesRegex(SystemExit, "not bound to RPC method GetCompare"):
                verify(root, snapshot)

    def test_mismatched_snapshot_fails(self) -> None:
        temporary, root, _ = self.fixture()
        with temporary, self.assertRaisesRegex(SystemExit, "snapshot differs"):
            verify(root, b"stale")

    def test_network_error_fails_closed(self) -> None:
        temporary, root, _ = self.fixture()
        with temporary:
            def unavailable(*_args: object, **_kwargs: object) -> object:
                raise urllib.error.URLError("offline")
            with self.assertRaisesRegex(SystemExit, "could not retrieve"):
                verify(root, opener=unavailable)


if __name__ == "__main__":
    unittest.main()
