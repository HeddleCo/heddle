# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.29.0](https://github.com/HeddleCo/heddle/compare/heddle-repo-v0.28.14...heddle-repo-v0.29.0) - 2026-10-09

### Other

- Fail on unavailable historical trees and blobs ([#2038](https://github.com/HeddleCo/heddle/pull/2038)) ([#2046](https://github.com/HeddleCo/heddle/pull/2046))
- Bound native Fetch source memory with disk-backed traversal ([#2045](https://github.com/HeddleCo/heddle/pull/2045))
- Refuse untrusted nested repositories at discovery ([#2034](https://github.com/HeddleCo/heddle/pull/2034)) ([#2037](https://github.com/HeddleCo/heddle/pull/2037))
- Check out any symlink target; never write through a symlink ([#2017](https://github.com/HeddleCo/heddle/pull/2017)) ([#2039](https://github.com/HeddleCo/heddle/pull/2039))
- Reserve .git/.heddle aliases as tree entry names on import and checkout ([#2028](https://github.com/HeddleCo/heddle/pull/2028)) ([#2033](https://github.com/HeddleCo/heddle/pull/2033))

## [0.28.14](https://github.com/HeddleCo/heddle/compare/heddle-repo-v0.28.13...heddle-repo-v0.28.14) - 2026-10-08

### Other

- receive imported ancestry on Fetch, stop the visibility walk at the import floor, fast adoption compression ([#2004](https://github.com/HeddleCo/heddle/pull/2004)) ([#2005](https://github.com/HeddleCo/heddle/pull/2005))
- Fix CI fixtures that capture without a principal, and the coverage feature name. ([#2001](https://github.com/HeddleCo/heddle/pull/2001))

## [0.28.13](https://github.com/HeddleCo/heddle/compare/heddle-repo-v0.28.12...heddle-repo-v0.28.13) - 2026-10-08

### Other

- receive imported ancestry on Fetch, stop the visibility walk at the import floor, fast adoption compression ([#2004](https://github.com/HeddleCo/heddle/pull/2004)) ([#2005](https://github.com/HeddleCo/heddle/pull/2005))
- Fix CI fixtures that capture without a principal, and the coverage feature name. ([#2001](https://github.com/HeddleCo/heddle/pull/2001))

## [0.28.12](https://github.com/HeddleCo/heddle/compare/heddle-repo-v0.28.11...heddle-repo-v0.28.12) - 2026-10-07

### Other

- receive imported ancestry on Fetch, stop the visibility walk at the import floor, fast adoption compression ([#2004](https://github.com/HeddleCo/heddle/pull/2004)) ([#2005](https://github.com/HeddleCo/heddle/pull/2005))
- Fix CI fixtures that capture without a principal, and the coverage feature name. ([#2001](https://github.com/HeddleCo/heddle/pull/2001))

## [0.26.0](https://github.com/HeddleCo/heddle/compare/heddle-repo-v0.25.7...heddle-repo-v0.26.0) - 2026-09-28

### Changed

- Require signature-v1 Biscuit blocks in local owner and thread evidence.

## [0.25.7](https://github.com/HeddleCo/heddle/compare/heddle-repo-v0.25.6...heddle-repo-v0.25.7) - 2026-09-28

### Other

- Update the capability-verifier dependency to 0.21.6.

## [0.25.6](https://github.com/HeddleCo/heddle/compare/heddle-repo-v0.25.5...heddle-repo-v0.25.6) - 2026-09-28

### Other

- recover timeline uploads with fresh acceptance and bound all pending evidence

## [0.25.5](https://github.com/HeddleCo/heddle/compare/heddle-repo-v0.25.4...heddle-repo-v0.25.5) - 2026-09-27

### Other

- update Cargo.toml dependencies

## [0.25.4](https://github.com/HeddleCo/heddle/compare/heddle-repo-v0.25.3...heddle-repo-v0.25.4) - 2026-09-25

### Other

- update Cargo.toml dependencies

## [0.25.3] - 2026-09-25

### Fixed

- restrict run timelines to owner and run principal (#1829)

### Other

- Make device run timelines readable and follow newest records (#1821)

### Other

- *(deps)* consume the Heddle-hosted capability verifier 0.20.0
  (heddle-api 0.31.0-alpha.1)
- *(deps)* adopt heddleco-capability-verifier 0.19.0 (heddle-api 0.30.0)

## [0.15.3](https://github.com/HeddleCo/heddle/compare/heddle-repo-v0.15.2...heddle-repo-v0.15.3) - 2026-08-28

### Other

- *(deps)* adopt heddle-api 0.18

## [0.15.2](https://github.com/HeddleCo/heddle/compare/heddle-repo-v0.15.1...heddle-repo-v0.15.2) - 2026-08-28

### Added

- productionize incremental HLR1/HDC1 tree storage ([#1587](https://github.com/HeddleCo/heddle/pull/1587))

### Other

- Discuss anchors: rebind on in-file symbol rename ([#1581](https://github.com/HeddleCo/heddle/pull/1581))
- Rematch context anchors across file renames ([#1580](https://github.com/HeddleCo/heddle/pull/1580))
- Add last-turn diff base ([#1582](https://github.com/HeddleCo/heddle/pull/1582))

## [0.15.1](https://github.com/HeddleCo/heddle/compare/heddle-repo-v0.15.0...heddle-repo-v0.15.1) - 2026-08-27

### Fixed

- close PR #1532 review findings ([#1561](https://github.com/HeddleCo/heddle/pull/1561))

### Other

- Headless agent create succeeds with claim directive ([#1572](https://github.com/HeddleCo/heddle/pull/1572))
- Identity cursor stamp for Claude, Codex, and OpenCode ([#1519](https://github.com/HeddleCo/heddle/pull/1519))
- Make Tree objects streamable and range-resumable ([#1471](https://github.com/HeddleCo/heddle/pull/1471))
- whole-CLI refactor in one shot — wave 0 + Wave-1 (−6.9k LOC) ([#1532](https://github.com/HeddleCo/heddle/pull/1532))
- verified via gate reproduction (sweep 2026-08-24)
