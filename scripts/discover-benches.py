#!/usr/bin/env python3
"""Emit benchmark targets as TSV: package, bench, comma-separated features."""

from pathlib import Path
import tomllib


def unique(values):
    seen = set()
    result = []
    for value in values:
        if value not in seen:
            seen.add(value)
            result.append(value)
    return result


for manifest in sorted(Path("crates").glob("*/Cargo.toml")):
    data = tomllib.loads(manifest.read_text())
    benches = data.get("bench", [])
    if not benches:
        continue

    package = data["package"]["name"]
    package_features = set(data.get("features", {}))
    for bench in benches:
        features = unique(bench.get("required-features", []))
        if "zstd" in package_features and "zstd" not in features:
            features.append("zstd")
        print(f"{package}\t{bench['name']}\t{','.join(features)}")
