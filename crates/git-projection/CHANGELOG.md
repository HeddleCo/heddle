# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.30.0](https://github.com/HeddleCo/heddle/compare/heddle-git-projection-v0.29.0...heddle-git-projection-v0.30.0) - 2026-10-10

### Other

- update Cargo.toml dependencies

## [0.29.0](https://github.com/HeddleCo/heddle/compare/heddle-git-projection-v0.28.14...heddle-git-projection-v0.29.0) - 2026-10-09

### Fixed

- *(git)* reconstruct -0000 timezone and empty author name byte-exactly ([#2016](https://github.com/HeddleCo/heddle/pull/2016)) ([#2041](https://github.com/HeddleCo/heddle/pull/2041))

### Other

- Fail on unavailable historical trees and blobs ([#2038](https://github.com/HeddleCo/heddle/pull/2038)) ([#2046](https://github.com/HeddleCo/heddle/pull/2046))

## [0.28.14](https://github.com/HeddleCo/heddle/compare/heddle-git-projection-v0.28.13...heddle-git-projection-v0.28.14) - 2026-10-08

### Other

- update Cargo.toml dependencies

## [0.25.4](https://github.com/HeddleCo/heddle/compare/heddle-git-projection-v0.25.3...heddle-git-projection-v0.25.4) - 2026-09-25

### Other

- Merge origin/main into embargo notes rebuild release branch

## [0.25.3] - 2026-09-25

### Other

- Fix wider CLI nextest suites and gate all CLI tests (#1826)

### Changed

- Smart-HTTP authoritative-ref push now reconciles and executes from one
  operation-scoped Sley 0.9 receive-pack observation.

## [0.15.1](https://github.com/HeddleCo/heddle/compare/heddle-git-projection-v0.15.0...heddle-git-projection-v0.15.1) - 2026-08-27

### Fixed

- close PR #1532 review findings ([#1561](https://github.com/HeddleCo/heddle/pull/1561))

### Other

- whole-CLI refactor in one shot — wave 0 + Wave-1 (−6.9k LOC) ([#1532](https://github.com/HeddleCo/heddle/pull/1532))
