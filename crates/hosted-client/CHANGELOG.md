# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Sign `CreateDeviceAuthorization` and `RegisterPublicKey` with the enrolling
  device key (`principal:device-key:<hex>`) and send only
  `device_proof_public_key` on RegisterPublicKey (weft#2047 one-key cutover).
- Hash Push/Pull `StreamOpeningProof.stream_id` to 64-byte blake3 hex so weft admits the opening (the long transfer identity stays on the checkpoint).

## [0.15.3](https://github.com/HeddleCo/heddle/compare/heddle-hosted-client-v0.15.2...heddle-hosted-client-v0.15.3) - 2026-08-28

### Fixed

- *(agent)* allow host-only spool provisioning in the agent ceiling ([#1592](https://github.com/HeddleCo/heddle/pull/1592))

### Other

- *(deps)* adopt heddle-api 0.18

## [0.15.2](https://github.com/HeddleCo/heddle/compare/heddle-hosted-client-v0.15.1...heddle-hosted-client-v0.15.2) - 2026-08-28

### Other

- Discuss anchors: rebind on in-file symbol rename ([#1581](https://github.com/HeddleCo/heddle/pull/1581))
- Rematch context anchors across file renames ([#1580](https://github.com/HeddleCo/heddle/pull/1580))
- Stop claim links from following unbound remote web_origin ([#1575](https://github.com/HeddleCo/heddle/pull/1575))

## [0.15.1](https://github.com/HeddleCo/heddle/compare/heddle-hosted-client-v0.15.0...heddle-hosted-client-v0.15.1) - 2026-08-27

### Fixed

- close PR #1532 review findings ([#1561](https://github.com/HeddleCo/heddle/pull/1561))

### Other

- Headless agent create succeeds with claim directive ([#1572](https://github.com/HeddleCo/heddle/pull/1572))
- Wire resident agent claim ceremony ([#1569](https://github.com/HeddleCo/heddle/pull/1569))
- Drop identity CLI; auth login is the only hosted-auth write ([#1517](https://github.com/HeddleCo/heddle/pull/1517))
