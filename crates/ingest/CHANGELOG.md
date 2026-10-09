# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.29.0](https://github.com/HeddleCo/heddle/compare/heddle-ingest-v0.28.14...heddle-ingest-v0.29.0) - 2026-10-09

### Fixed

- *(git)* reconstruct -0000 timezone and empty author name byte-exactly ([#2016](https://github.com/HeddleCo/heddle/pull/2016)) ([#2041](https://github.com/HeddleCo/heddle/pull/2041))

### Other

- Fail on unavailable historical trees and blobs ([#2038](https://github.com/HeddleCo/heddle/pull/2038)) ([#2046](https://github.com/HeddleCo/heddle/pull/2046))
- Reserve .git/.heddle aliases as tree entry names on import and checkout ([#2028](https://github.com/HeddleCo/heddle/pull/2028)) ([#2033](https://github.com/HeddleCo/heddle/pull/2033))

## [0.28.14](https://github.com/HeddleCo/heddle/compare/heddle-ingest-v0.28.13...heddle-ingest-v0.28.14) - 2026-10-08

### Other

- update Cargo.toml dependencies

## [0.28.13](https://github.com/HeddleCo/heddle/compare/heddle-ingest-v0.28.12...heddle-ingest-v0.28.13) - 2026-10-08

### Other

- update Cargo.toml dependencies

## [0.15.1](https://github.com/HeddleCo/heddle/compare/heddle-ingest-v0.15.0...heddle-ingest-v0.15.1) - 2026-08-27

### Fixed

- close PR #1532 review findings ([#1561](https://github.com/HeddleCo/heddle/pull/1561))

### Other

- Make Tree objects streamable and range-resumable ([#1471](https://github.com/HeddleCo/heddle/pull/1471))
- whole-CLI refactor in one shot — wave 0 + Wave-1 (−6.9k LOC) ([#1532](https://github.com/HeddleCo/heddle/pull/1532))
