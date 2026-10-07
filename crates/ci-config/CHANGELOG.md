# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.28.12](https://github.com/HeddleCo/heddle/compare/heddle-ci-config-v0.28.11...heddle-ci-config-v0.28.12) - 2026-10-07

### Other

- update Cargo.toml dependencies

## [0.25.6](https://github.com/HeddleCo/heddle/compare/heddle-ci-config-v0.25.5...heddle-ci-config-v0.25.6) - 2026-09-28

### Other

- update Cargo.toml dependencies

## [0.25.5](https://github.com/HeddleCo/heddle/compare/heddle-ci-config-v0.25.4...heddle-ci-config-v0.25.5) - 2026-09-27

### Other

- update Cargo.toml dependencies

## [0.25.4](https://github.com/HeddleCo/heddle/compare/heddle-ci-config-v0.25.3...heddle-ci-config-v0.25.4) - 2026-09-25

### Other

- update Cargo.toml dependencies

### Changed

- The on-disk contract is a canonical `TreadleDefinition` protobuf
  (`.heddle/treadle.definition.bin`) plus required `treadle.lock.json`.
  TOML is no longer a definition language.
- Host-exec refuses isolation the host cannot apply (`cpu_millis`,
  `memory_bytes`, `process_limit`, named profile). `network_access = NONE`
  remains admitted. `cache_paths` persist worktree-relative directories.
