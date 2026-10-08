# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.28.13](https://github.com/HeddleCo/heddle/compare/heddleco-capability-verifier-v0.28.12...heddleco-capability-verifier-v0.28.13) - 2026-10-08

### Other

- receive imported ancestry on Fetch, stop the visibility walk at the import floor, fast adoption compression ([#2004](https://github.com/HeddleCo/heddle/pull/2004)) ([#2005](https://github.com/HeddleCo/heddle/pull/2005))

## [0.28.12](https://github.com/HeddleCo/heddle/compare/heddleco-capability-verifier-v0.28.11...heddleco-capability-verifier-v0.28.12) - 2026-10-07

### Other

- receive imported ancestry on Fetch, stop the visibility walk at the import floor, fast adoption compression ([#2004](https://github.com/HeddleCo/heddle/pull/2004)) ([#2005](https://github.com/HeddleCo/heddle/pull/2005))

### Changed

- Release 0.28.11: HYBRID Part 8 adopts api alpha.37 writer authority and foreign closures (#1988); Part 9 pins the receiver owner and adopts api alpha.39 (invitations by handle, notification inheritance, role approval groups, SuggestPrincipals, and invite codes) (#1989); Part 10 stops Fetch at verified foreign endpoints and fixes false rollback reports from receiver clock sampling (#1990). CI moves to Blacksmith (#1986, #1987).

- Release 0.28.10: Windows builds compile again; fix git-projection `GitError::io_kind` and hosted-client Unix bridge cfg (#1982).

- Release 0.28.9: content readers handle the mandatory alpha.22 AcceptedBudget echo first, exactly once, validated and budget-accounted (#1979).

- PR #1965 review fixes: validate original JS bigint types and signed/unsigned ranges before WASM conversion; exclude all verified owner-history authority keys when a policy introduces revocations; use the general 256-transition import bound and restore the 4096 policy-revocation bound. Keep the alpha.21 API pin; alpha.23 error mapping and dependency migration remain coordinated release work.

- HYBRID Part 3 (#1961, weft#2469): move complete signed Spool policy verification and tests from weft into the public Rust `policy` API; reuse it for import provenance. Expose production WASM transfer/audit, resource-keyring, self/delegated genesis and policy-chain APIs with typed JS objects and error codes. Scope historical policy keys to verified ownership phases. Publish `@heddleco/capability-verifier-wasm` to GitHub Packages from the stable Heddle release workflow at the matching workspace version; build generated artifacts in CI.

- Adopt heddle-api 0.31.0-alpha.21 and the published boundary vectors. Reject witness and permanently known job keys in delegator/user positions, preserve milliseconds for historical freshness, and exercise the public import JS/WASM binding against native digests and rejection reasons. Release with the workspace and npm package at 0.28.8 (#1961, #1964). Alpha.21 (api#322) revises undeployed Prepare/Commit in place; completed signing layouts and the alpha.20 boundary binding are unchanged. Part 2 owns the client/RPC integration.

- Adopt heddle-api 0.31.0-alpha.19 (PlatformAdminService checks, notification contract, identity account management; additive) and release with the workspace and npm package at 0.28.7; no verifier behavior change.
- Adopt heddle-api 0.31.0-alpha.18 (hybrid import-authority / host-witness contract, catalog filter, owner/address search and CatalogSummary, custodial 1-of-1 recovery RPCs and the `custodial_email` recovery proof, bookmark timestamps, and the optional signed `SourceAnchor.path_kind` extension; additive) and release with the workspace and npm package at 0.28.6; no verifier behavior change.
- Adopt heddle-api 0.31.0-alpha.16 (operation context, landing-assessment status, spool default thread, client SemanticIndex ingestion messages, code-navigation reads, tip-only search docs, ListPaths, SpoolOverview.ancestors, author display names, provider default branch/refs, stable capability enum plus Blocked.error, search-hit thread name and spool path, relationship name and lifecycle; additive) and release with the workspace and npm package at 0.28.5; no verifier behavior change.
- **BREAKING (verifier behavior):** Recover now requires a next recovery policy (`next_recovery_policy` plus `next_recovery_key_proofs`) and fails closed without one. The retained old guardians must fall below the new policy's threshold, so the guardian words that authorized a recovery cannot start another. Release with the workspace and npm package at 0.28.4 (heddle#1952, weft#1527).
- Require Recover to install a replacement recovery policy with every next guardian's transition-bound possession proof. Current guardians authorize recovery; the old guardian set cannot satisfy the new threshold. Native Rust and WASM/npm reject legacy Recover records without next-policy proofs. Regenerate recovery and timeline vectors for this contract.
- Adopt heddle-api 0.31.0-alpha.13 (blocking-discussion resolve rule, additive) and release with the workspace and npm package at 0.28.3; no verifier behavior change.
- Release with the workspace and npm package at 0.28.2; no verifier behavior change. The workspace carries signature-to-Thread binding fixes for context and discussion records (heddle#1939, #1941) and the merge-commit fix for `heddle import local` (#1942).
- Pin heddle-api 0.31.0-alpha.12 and use its 64 KiB timeline owner-bundle bound in native and WASM verification, including the conformance and browser binding envelopes. Release with the workspace and npm package at 0.28.1 as required by the dependency-version guard.
- Version capability-verifier and its npm/WASM package with the workspace (0.27.2); require every publishable crate to inherit the workspace version.
- Bump the workspace to 0.27.2 because the dependency-version guard requires an increase for updated capability-verifier requirements.
- Pin heddle-api 0.31.0-alpha.11 and release with the workspace at 0.28.0; the verifier uses none of the new fields.

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
