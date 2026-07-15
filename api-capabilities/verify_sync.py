#!/usr/bin/env python3
"""Validate Heddle evidence and its immutable sanitized API snapshot."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import urllib.error
import urllib.request
from pathlib import Path
from typing import Callable


ROOT = Path(__file__).resolve().parent
REPOSITORY_ROOT = ROOT.parent
CONSUMER = "heddle"
LOCAL_DECLARATION = "heddle.json"
API_REPOSITORY = "HeddleCo/api"
API_REVISION = "5c9bbe11dd7b09b9825a275d247a796d4da868f4"
API_SNAPSHOT = "capabilities/declarations/heddle.json"
LAYERS = ("client", "cli")
STATUSES = {
    "shipped",
    "partial",
    "planned",
    "intentionally-unsupported",
    "blocked",
}


def _sha256(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


def _snake_case(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def _source_text(repository_root: Path, evidence: object, rpc: str) -> str:
    if not isinstance(evidence, dict) or set(evidence) != {"path", "contains"}:
        raise SystemExit(f"invalid source evidence for {rpc}")
    relative = evidence["path"]
    needle = evidence["contains"]
    if not isinstance(relative, str) or not isinstance(needle, str) or not needle:
        raise SystemExit(f"invalid source evidence for {rpc}")
    path = Path(relative)
    if path.is_absolute() or ".." in path.parts:
        raise SystemExit(f"unsafe evidence path for {rpc}")
    source = repository_root / path
    if not source.is_file():
        raise SystemExit(f"evidence path does not exist for {rpc}: {relative}")
    text = source.read_text(errors="replace")
    if needle not in text:
        raise SystemExit(f"evidence symbol/call edge is missing for {rpc}: {needle}")
    return needle


def sanitize(declaration: object, repository_root: Path) -> dict[str, object]:
    """Validate detailed local mappings and remove all private evidence fields."""
    if not isinstance(declaration, dict):
        raise SystemExit("local declaration must be an object")
    if declaration.get("schema_version") != 2:
        raise SystemExit("unsupported local declaration schema")
    if declaration.get("consumer") != CONSUMER:
        raise SystemExit("unexpected local declaration consumer")
    if declaration.get("source_repository") != "HeddleCo/heddle":
        raise SystemExit("unexpected local declaration repository")
    rows = declaration.get("rpc_mappings")
    if not isinstance(rows, list):
        raise SystemExit("rpc_mappings must be a list")

    sanitized_rows: list[dict[str, object]] = []
    seen: set[str] = set()
    for row in rows:
        if not isinstance(row, dict) or not isinstance(row.get("rpc"), str):
            raise SystemExit("invalid RPC mapping")
        rpc = row["rpc"]
        if rpc in seen:
            raise SystemExit(f"duplicate RPC mapping: {rpc}")
        seen.add(rpc)
        capability = row.get("capability")
        layers = row.get("layers")
        if not isinstance(capability, str) or not capability:
            raise SystemExit(f"invalid capability for {rpc}")
        if not isinstance(layers, dict) or set(layers) != set(LAYERS):
            raise SystemExit(f"invalid layer set for {rpc}")
        sanitized_layers: dict[str, dict[str, str]] = {}
        method = rpc.rsplit("/", 1)[-1]
        expected_tokens = (method, _snake_case(method))
        for layer_name in LAYERS:
            layer = layers[layer_name]
            if not isinstance(layer, dict) or layer.get("status") not in STATUSES:
                raise SystemExit(f"invalid {layer_name} status for {rpc}")
            status = layer["status"]
            evidence = layer.get("evidence")
            if not isinstance(evidence, list):
                raise SystemExit(f"invalid {layer_name} evidence for {rpc}")
            if status in {"shipped", "partial"}:
                if not evidence:
                    raise SystemExit(f"missing {layer_name} evidence for {rpc}")
                for item in evidence:
                    needle = _source_text(repository_root, item, rpc)
                    if not any(token in needle for token in expected_tokens):
                        raise SystemExit(
                            f"{layer_name} evidence is not bound to RPC method {method}"
                        )
            sanitized_layers[layer_name] = {"status": status}
        sanitized_rows.append(
            {"rpc": rpc, "capability": capability, "layers": sanitized_layers}
        )
    return {
        "schema_version": 2,
        "consumer": CONSUMER,
        "rpc_mappings": sanitized_rows,
    }


def sanitized_bytes(declaration: object, repository_root: Path) -> bytes:
    return (json.dumps(sanitize(declaration, repository_root), indent=2) + "\n").encode()


def verify(
    root: Path,
    snapshot: bytes | None = None,
    opener: Callable[..., object] = urllib.request.urlopen,
) -> None:
    provenance = json.loads((root / "provenance.json").read_text())
    expected = {
        "schema_version": 2,
        "consumer": CONSUMER,
        "api_repository": API_REPOSITORY,
        "api_revision": API_REVISION,
        "api_snapshot": API_SNAPSHOT,
        "local_declaration": LOCAL_DECLARATION,
    }
    if not isinstance(provenance, dict) or any(
        provenance.get(key) != value for key, value in expected.items()
    ):
        raise SystemExit("provenance identity, path, or revision differs from the pinned contract")
    if set(provenance) != set(expected) | {"local_sha256", "sanitized_sha256"}:
        raise SystemExit("unexpected provenance fields")

    local = (root / LOCAL_DECLARATION).read_bytes()
    if provenance.get("local_sha256") != _sha256(local):
        raise SystemExit("local declaration attestation differs from content")
    derived = sanitized_bytes(json.loads(local), root.parent)
    if provenance.get("sanitized_sha256") != _sha256(derived):
        raise SystemExit("sanitized declaration attestation differs from derived content")

    if snapshot is None:
        url = f"https://raw.githubusercontent.com/{API_REPOSITORY}/{API_REVISION}/{API_SNAPSHOT}"
        try:
            with opener(url, timeout=30) as response:  # type: ignore[attr-defined]
                snapshot = response.read()
        except (OSError, urllib.error.URLError) as error:
            raise SystemExit(f"could not retrieve immutable API snapshot: {error}") from error
    if snapshot != derived or _sha256(snapshot) != provenance["sanitized_sha256"]:
        raise SystemExit("immutable API snapshot differs from derived sanitized declaration")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--snapshot", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    verify(ROOT, args.snapshot.read_bytes() if args.snapshot else None)


if __name__ == "__main__":
    main()
