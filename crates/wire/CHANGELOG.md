# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.30.0](https://github.com/HeddleCo/heddle/compare/heddle-wire-v0.29.0...heddle-wire-v0.30.0) - 2026-10-10

### Other

- update Cargo.toml dependencies

## [0.29.0](https://github.com/HeddleCo/heddle/compare/heddle-wire-v0.28.14...heddle-wire-v0.29.0) - 2026-10-09

### Other

- Bound native Fetch source memory with disk-backed traversal ([#2045](https://github.com/HeddleCo/heddle/pull/2045))

## [0.28.14](https://github.com/HeddleCo/heddle/compare/heddle-wire-v0.28.13...heddle-wire-v0.28.14) - 2026-10-08

### Other

- update Cargo.toml dependencies

## [0.25.3] - 2026-09-25

### Other

- Make device run timelines readable and follow newest records (#1821)

## [0.15.2](https://github.com/HeddleCo/heddle/compare/heddle-wire-v0.15.1...heddle-wire-v0.15.2) - 2026-08-28

### Added

- productionize incremental HLR1/HDC1 tree storage ([#1587](https://github.com/HeddleCo/heddle/pull/1587))

### Fixed

- *(objects)* bound HTR4 v5 block raw_len to prevent decompression OOM ([#1589](https://github.com/HeddleCo/heddle/pull/1589))

## [0.15.1](https://github.com/HeddleCo/heddle/compare/heddle-wire-v0.15.0...heddle-wire-v0.15.1) - 2026-08-27

### Fixed

- close PR #1532 review findings ([#1561](https://github.com/HeddleCo/heddle/pull/1561))

### Other

- Make Tree objects streamable and range-resumable ([#1471](https://github.com/HeddleCo/heddle/pull/1471))
- whole-CLI refactor in one shot — wave 0 + Wave-1 (−6.9k LOC) ([#1532](https://github.com/HeddleCo/heddle/pull/1532))
