# Changelog

## [Unreleased]

## [0.30.1](https://github.com/HeddleCo/heddle/compare/heddle-git-serialize-v0.30.0...heddle-git-serialize-v0.30.1) - 2026-10-12

### Added

- *(git)* add engine-free Git reconstruction API ([#2064](https://github.com/HeddleCo/heddle/pull/2064)) ([#2065](https://github.com/HeddleCo/heddle/pull/2065))

- Pure, byte-exact commit and tree serializers, zero-copy blob passthrough and
  Git object hashing for SHA-1 and SHA-256.
- Store-independent async closure reconstruction with bounded iterative walks,
  a caller-supplied reader and sink, and explicit resource limits.
