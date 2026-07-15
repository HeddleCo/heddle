#!/usr/bin/env python3
"""Validate Heddle evidence and its immutable sanitized API snapshot."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import urllib.error
import urllib.request
from functools import lru_cache
from pathlib import Path
from typing import Callable


ROOT = Path(__file__).resolve().parent
REPOSITORY_ROOT = ROOT.parent
CONSUMER = "heddle"
LOCAL_DECLARATION = "heddle.json"
API_REPOSITORY = "HeddleCo/api"
API_REVISION = "461099c41cea91357c022ea0b21a88c8bf08aa60"
API_SNAPSHOT = "capabilities/declarations/heddle.json"
LAYERS = ("client", "cli")
STATUSES = {
    "shipped",
    "partial",
    "planned",
    "intentionally-unsupported",
    "blocked",
}
RAW_RUST_STRING = re.compile(r'(?:br|cr|r)(?P<hashes>#{0,255})"')
RUST_CHAR = re.compile(
    r"'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]{1,6}\}|[^\n])|[^\\'\n])'"
)


def _sha256(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


def _snake_case(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


@lru_cache(maxsize=64)
def _rust_code(text: str) -> str:
    """Mask Rust comments and literals while preserving source positions."""
    output: list[str] = []
    index = 0
    length = len(text)

    def mask(start: int, end: int) -> None:
        output.extend("\n" if char == "\n" else " " for char in text[start:end])

    while index < length:
        raw = RAW_RUST_STRING.match(text, index)
        if raw is not None:
            closing = '"' + raw.group("hashes")
            end = text.find(closing, raw.end())
            end = length if end < 0 else end + len(closing)
            mask(index, end)
            index = end
            continue
        if text[index] == '"':
            start = index
            index += 1
            while index < length:
                if text[index] == "\\":
                    index = min(index + 2, length)
                    continue
                index += 1
                if text[index - 1] == '"':
                    break
            mask(start, index)
            continue
        char = RUST_CHAR.match(text, index)
        if char is not None:
            mask(index, char.end())
            index = char.end()
            continue
        if text.startswith("//", index):
            end = text.find("\n", index + 2)
            end = length if end < 0 else end
            mask(index, end)
            index = end
            continue
        if text.startswith("/*", index):
            start = index
            depth = 1
            index += 2
            while index < length and depth:
                if text.startswith("/*", index):
                    depth += 1
                    index += 2
                elif text.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
            mask(start, index)
            continue
        output.append(text[index])
        index += 1
    return "".join(output)


def _source_text(repository_root: Path, evidence: object, rpc: str) -> tuple[str, str]:
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
    return needle, source.read_text(errors="replace")


def _validate_rust_binding(
    repository_root: Path,
    evidence: object,
    rpc: str,
    layer_name: str,
) -> None:
    """Require an actual Rust function definition or call, never a name substring."""
    needle, source_text = _source_text(repository_root, evidence, rpc)
    text = _rust_code(source_text)
    method = rpc.rsplit("/", 1)[-1]
    canonical = _snake_case(method)
    if needle == canonical:
        binding = re.compile(
            rf"(?:\basync\s+fn\s+{re.escape(canonical)}\s*\(|\.{re.escape(canonical)}\s*\()"
        )
    else:
        symbol = re.search(r"\.([a-z][a-z0-9_]*)\s*\($", needle)
        if symbol is None or not (
            symbol.group(1) == canonical
            or symbol.group(1).startswith(canonical + "_")
        ):
            raise SystemExit(f"{layer_name} evidence is not bound to RPC method {method}")
        binding = re.compile(re.escape(needle))
    if binding.search(text) is None:
        raise SystemExit(f"structural {layer_name} binding is missing for {rpc}")


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
        if (
            not isinstance(row, dict)
            or set(row) != {"rpc", "layers"}
            or not isinstance(row.get("rpc"), str)
        ):
            raise SystemExit("invalid RPC mapping")
        rpc = row["rpc"]
        if rpc in seen:
            raise SystemExit(f"duplicate RPC mapping: {rpc}")
        seen.add(rpc)
        layers = row.get("layers")
        if not isinstance(layers, dict) or set(layers) != set(LAYERS):
            raise SystemExit(f"invalid layer set for {rpc}")
        sanitized_layers: dict[str, dict[str, str]] = {}
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
                    _validate_rust_binding(repository_root, item, rpc, layer_name)
            sanitized_layers[layer_name] = {"status": status}
        sanitized_rows.append({"rpc": rpc, "layers": sanitized_layers})
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
