# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.30.0](https://github.com/HeddleCo/heddle/compare/heddle-config-v0.29.0...heddle-config-v0.30.0) - 2026-10-10

### Other

- update Cargo.toml dependencies

## [0.29.0](https://github.com/HeddleCo/heddle/compare/heddle-config-v0.28.14...heddle-config-v0.29.0) - 2026-10-09

### Other

- Refuse untrusted nested repositories at discovery ([#2034](https://github.com/HeddleCo/heddle/pull/2034)) ([#2037](https://github.com/HeddleCo/heddle/pull/2037))

### Added

- `ClientConfig::preferred_region` prefers same-region root-attested
  endpoints when dialing a leaderless weft fleet (heddle#1566).
