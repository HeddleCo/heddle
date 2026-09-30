# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Version capability-verifier and its npm/WASM package with the workspace (0.27.2); require every publishable crate to inherit the workspace version.
- Bump the workspace to 0.27.2 because the dependency-version guard requires an increase for updated capability-verifier requirements.

## [0.23.0](https://github.com/HeddleCo/heddle/compare/heddleco-capability-verifier-v0.22.0...heddleco-capability-verifier-v0.23.0) - 2026-09-30

### Changed

- Pin heddle-api 0.31.0-alpha.10 and heddle-biscuit-verifier 0.27.1.
- Release matching Rust and npm versions so registry consumers receive the updated dependency contract.

## [0.22.0](https://github.com/HeddleCo/heddle/compare/heddleco-capability-verifier-v0.21.6...heddleco-capability-verifier-v0.22.0) - 2026-09-28

### Changed

- Verify signature-v1 subject Biscuits and sealed owner evidence through the shared verifier.
- Pin heddle-api 0.31.0-alpha.8.

## [0.21.6](https://github.com/HeddleCo/heddle/compare/heddleco-capability-verifier-v0.21.5...heddleco-capability-verifier-v0.21.6) - 2026-09-28

### Other

- Verify exact format-3 timeline owner capabilities and acceptance in Rust and WASM.

## [0.21.5](https://github.com/HeddleCo/heddle/compare/heddleco-capability-verifier-v0.21.4...heddleco-capability-verifier-v0.21.5) - 2026-09-28

### Other

- pin heddle-api 0.31.0-alpha.6

## [0.21.4](https://github.com/HeddleCo/heddle/compare/heddleco-capability-verifier-v0.21.3...heddleco-capability-verifier-v0.21.4) - 2026-09-27

### Other

- *(deps)* pin heddle-api 0.31.0-alpha.5

## [0.21.3](https://github.com/HeddleCo/heddle/compare/heddleco-capability-verifier-v0.21.2...heddleco-capability-verifier-v0.21.3) - 2026-09-25

### Other

- *(deps)* bump heddle-api to 0.31.0-alpha.3

## [0.21.2] - 2026-09-25

### Other

- updated the following local packages: heddle-biscuit-verifier
