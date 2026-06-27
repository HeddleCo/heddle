# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog. Public releases follow semver
(starting with `0.2.0`); pre-release entries below the public release are
grouped by date to capture the internal development history.

Only changes that affect the OSS CLI and its supporting crates are
recorded here. Hosted-product work (Postgres, Biscuit, the web app,
GitHub App, etc.) lives in the closed `HeddleCo/weft` and
`HeddleCo/tapestry` repos.

## Unreleased

## 0.5.1 - 2026-06-27

### Changed

- Upgraded the native-git substrate to sley `0.3.1` and switched published
  Heddle crates back to the crates.io dependency instead of the sibling path.
  sley `0.3.1` adds a receive-pack input-size cap and unix-portability
  cfg-gates.
- Hosted Git-overlay sync now sends Git-shaped data through the Git lane as
  packs, while Heddle-native captures, state, context, discussions, and
  visibility metadata remain on the Heddle lane.
- `heddle-grpc` and `@heddleco/grpc` move to `0.7.2` with the Git lane pack
  transfer protobuf surface and hosted spool API additions.

### Fixed

- Heddle's Sley fetch call sites now pass the explicit `FetchOptions.atomic`
  setting required by sley `0.3.1`, preserving the previous non-atomic fetch
  behavior.
- `.git/HEAD` symref updates are now atomic (temp-file + rename + fsync),
  preventing a torn HEAD if the process is interrupted mid-write.
- A corrupt or unreadable user config now fails closed with a clear error
  instead of silently attributing captures/checkpoints to an
  `Unknown <unknown@example.com>` identity.
- A git-lane pull targeting a non-overlay repository now returns an error
  instead of silently discarding the transferred data.
- The fast short-status path now classifies staged-add-then-worktree-deleted
  (`AD`), renames (`R`), and copies (`C`) consistently with the full status
  path.
- The worktree change scan now detects a new file added to an already-tracked
  directory. Previously such a file could be missed by `status`/`capture`
  until an unrelated change forced a rescan.

### Security

- `heddle try` now sanitizes the spawned command's environment —
  `env_clear()` plus an allowlist, matching `heddle run` — so `GIT_DIR`,
  `GIT_WORK_TREE`, and other inherited parent-process secrets no longer leak
  into commands run via `try`.

## 0.5.0 - 2026-06-23

Native-git substrate jumps to sley 0.2.0, the storage seam lands, and the
embeddable `heddle-core` facade takes shape.

### Added

- **`heddle-core` embeddable facade.** New facade crate with an
  `ExecutionContext` and observability sinks, a query facade lifted out of
  the CLI, and a compute→render split exemplar (`fsck` computes a
  `FsckReport`; the CLI renders it). Groundwork for embedding heddle as a
  library. (#775, #780, #782)
- **New published crates `heddle-format` and `heddle-schema`**, extracted as
  pure (behaviour-neutral) cores from `heddle-objects` and now part of the
  crates.io publish set. (#761)

### Changed

- **Native-git substrate upgraded to sley `0.2.0`** (~24 parity waves since
  `0.1.0`: multi-pack-index, reftable basics, fetch, gc, blame, worktree,
  split-index, mergetool, the diff-format crate extraction, and more). (#784)
- **Storage seam.** Reads now flow through an `ObjectSource` sync/async seam;
  `query_history`/`CommitGraphIndex` are decoupled onto it with the cache as
  an optional accelerator; staleness and context-suggestion pure cores moved
  into `heddle-objects`. (#762, #763, #764)
- **`clap` confined to the `heddle-cli` crate.** `heddle-ingest` and the
  library crates no longer pull `clap`, shrinking the embeddable surface.
  (#781, #783)
- Faster status scans: borrow entry names instead of cloning. (#774)

### Removed

- The S3 object backend and its AWS dependency chain (a pre-seam leftover).
  (#765)

## 0.4.0 - 2026-06-19

### Changed

- **Breaking (crate surface):** renamed the native Heddle wire/protocol
  crate from `heddle-proto` (`crates/proto`, Rust crate `proto`) to
  `heddle-wire` (`crates/wire`, Rust crate `wire`). The protobuf/gRPC IDL
  remains in `heddle-grpc` under `crates/grpc/proto/heddle/v1`.

### Added

- `@heddle/grpc`: a generated TypeScript protobuf + Connect (v2) client
  package for the Heddle gRPC API, versioned with `heddle-grpc` (`0.7.1`),
  publishable to GitHub Packages for Tapestry consumption. New path-gated
  `ts-grpc-client` CI workflow validates/typechecks/packs it and gates
  publish to `main`.

## 0.3.0 - 2026-06-16

Git operations move onto a native-Rust substrate, the command surface is
consolidated, lossless git round-trip becomes a hard guarantee, and the
release ships signed macOS + Homebrew distribution.

### Breaking

- Semantic merge is the default merge strategy; the redundant `--semantic` flag is removed (#669)
- Verb-surface consolidation: ~85 top-level verbs reduced to ~28 canonical ones. Save verbs collapse onto `commit`/`ready`/`land`/`sync` (#478); redundant roots are deleted from the command tree, dispatch, catalog, and schemas, not just hidden (#681)
- `git import` refuses lossy conversions by default; unrepresentable tree entries now fail hard, with explicit `--lossy` opt-in (#453)
- `FullRematerialize` refuses to run against a dirty worktree by default; the destructive discard is opt-in (#442)
- `OutputMode::Auto` removed — output no longer silently switches to JSON when piped; `--output json` is explicit (#280)
- `OpRecord` v2 record variants collapsed and a schema-versioning migration added for legacy oplog repos (#670, #452)
- RPC rename `AuthService.FinishWebAuthnRegistration` → `RegisterPublicKey` (`crates/grpc/proto/heddle/v1/service.proto`; #63). The old name reflected WebAuthn ceremony state; the new name reflects what the call actually does — store a public key + attestation. `BeginWebAuthnRegistration` keeps its name (generic challenge-init); the request message is renamed in lockstep (`FinishWebAuthnRegistrationRequest` → `RegisterPublicKeyRequest`). All field numbers are preserved, so wire-compatible consumers that don't pin the message-type name keep decoding existing payloads, but consumers pinned to `^0.2` must update generated client/server stubs before upgrading

### Changed

- Squash-by-default land: a landed thread exports as one atomic Git commit while preserving per-State Heddle history; `--no-squash` and a `[land] squash` config override remain (#680)
- `heddle start --path` is the canonical isolated-checkout path; `thread create`/`thread promote` demoted off the advertised surface; thread checkouts land under `.heddle/threads/` for sandbox-friendliness (#685, #576)
- Object reads and hashing route through a native-Rust `git_substrate` adapter backed by [sley](https://crates.io/crates/sley) `0.0.3`; remaining `gix` usage is cut over so there is no git shell-out on the hot path (#733, #738, #595)
- `Repository` genericized over its ref/object backends — `dyn` dispatch replaced with type parameters, removing vtable overhead (#282, #293)
- Native ref primitives: `parse_git_ref` + `RefSpec`/`NegativeRefSpec` ported from jj; `.git/config` remotes parsed via `gix_config` instead of by hand (#296, #288)
- Captured new files are marked intent-to-add in the colocated Git index, matching `git add -N` semantics (#300)
- `output_kind` wire normalization across thread/operator envelopes and the read/stream paths (inspect/show, rebase JSONL, conflict-show), so `--output json` is stable end to end; an invariant lint guards the class (#671, #660, #281)
- Help and onboarding overhaul: one-command first-run onboarding (#349), grouped advanced command list, single-sourced `--output` help, `help threads`/`help advanced`/Git-concept topics, and de-bloated per-command help (#663, #658, #688, #174)

### Added

- `heddle oplog recover` / operator recover: reconstructs a truncated oplog from the EOF footer plus a salvage sidecar, so a truncated log no longer bricks a repo (#682, #736, #618)
- Packed oplog v3: single-file tail format with an EOF footer carrying entry-offset, batch, and sorted `transaction_id` indexes (#429)
- Lossless git round-trip: git-fidelity fields (committer, tz offsets, verbatim message, ordered extra headers, GPG sig) captured on import with a format bump (#569); a byte-exact `reconstruct_commit_bytes` serializer + conformance harness (#579); export reconstructs commit objects from state (#591); and a `bridge backfill-fidelity` migration for pre-bump repos (#587)
- Automatic ed25519 state signing on capture/commit/merge, wiring the device key into every recorded state (#486)
- Function-level semantic merge driver: AST-aware three-way merge plus a native hunk-level (diff3-style) text merger; Rust `use` re-exports merge as path-keyed items (#114, #84, #477)
- Cross-thread undo, undoable rebase (transaction grouping), `FastForward`/`FastForwardV2` merge undo, and a pre-undo recovery marker so undo never silently loses worktree edits (#112, #218, #109, #118, #314)
- `heddle expand <oid|state|thread>` reconstructs the constituent captures of a collapsed squashed land (#721)
- Partial clone: `clone --depth` and `clone --filter blob:none` (synonym for `--lazy`) wired through the Git-overlay clone, plus lazy on-read blob hydration via a `BlobHydrator` trait (#52, #51, #53)
- Dynamic shell completion (`__complete`) and a `heddle shell prompt` segment (#731)
- Structured `blame --output json` with per-line principal + agent attribution (#322)
- npm/TypeScript client: a transport-agnostic `Heddle` class over the CLI's JSON contract, backed by generated TS types and full JSON-contract coverage for harness ops (#592, #588)
- `daemon` transaction-replay state machine for crash recovery of leftover transaction sentinels (#47)
- `push --mirror` for ad-hoc dual-push (#227)
- Device-key binding-signature fields on `RegisterPublicKeyRequest` (`crates/grpc/proto/heddle/v1/service.proto`; tags 16/17/18). Let a client prove that the same WebAuthn authenticator that issued the attestation also signs the renewal-anchor `device_public_key`, closing a gap where a client could attach an unrelated Ed25519 key to a real attestation. The challenge the assertion signs is `base64url(SHA256("heddle-device-binding-v1" || 0x00 || device_public_key))` (#131)
  - `bytes device_binding_client_data_json = 16;`
  - `bytes device_binding_authenticator_data = 17;`
  - `bytes device_binding_signature = 18;`

### Fixed

- P0 semantic-merge corruption: containers modeled as first-class tree nodes so 3-way merge no longer silently mis-nests or erases subtrees (#506, #92)
- Conflict markers anchored at column 0; indented conflict-marker recognition fixed (#82, #724)
- Lossy attribution for cached git subtrees no longer double-counts on importer cache hits (#711)
- `undo` thread `base_state` restore corrected; thread-base refresh now routes through the oplog so it round-trips (#677)
- Mount/FUSE made to work end-to-end on Linux (three production-blocking bugs), with an out-of-process `heddle-fuse-worker` subprocess for callback isolation; Windows ProjFS production-hardened (#85, #225, #96)
- Fail-loud fixes across the read/sync/config paths: broken GitHub REST pagination, unreadable hydration state, and TLS/auth config-read failures now error instead of silently degrading (#450, #448, #446)
- Dozens of correctness and durability fixes across the oplog, pack, refs, and objects paths — checked arithmetic in the pack-size/offset decoder, per-entry content-hash validation on `install_pack`, advisory-lock stale-lock reaping, atomic transaction-sentinel writes, and HEAD reconstruction from Snapshot/Goto records (#396, #395, #447, #527, #394, and others)

### Security

- Path-traversal hardening: a `..`-traversal bypass in namespace/repo access checks closed via a shared segment-aware helper; namespace scope grants downward access only (#638, #628)
- Materialized symlink targets validated against the repo root (capture already checked them; materialization did not) (#430)
- Atomic secret-write primitive: temp files created `0600` before any bytes are written, with hard errors on bad perms (#428)
- Daemon hardening: `SO_PEERCRED` peer-identity enforcement, secured socket bind, and a validated `transaction_id` at the RPC boundary to close a path-traversal vector (#626, #699, #441)
- Bounded received-blob sizes on redaction / state-visibility / native-pack transfers to cap untrusted input (#530, #417)
- Supply-chain gate: `cargo-audit` + `cargo-deny` added to CI; dropped a yanked dependency and scrubbed real personal data from example/test fixtures (#40, #376)

### Performance

- `tree_diff` rewritten as a sorted merge-join with deterministic order and no per-entry allocations; internal-iteration `diff_trees_visit` for streaming/early-exit consumers (#422, #399, #635)
- `status` sped up: deduped redundant git-overlay passes, an index stat fast-path, and reduced `compare_worktree` `get_tree` overhead (#560, #556)
- Pack and push hot paths: zero-copy `install_pack` validation, eliminated whole-pack `to_vec()` clones, and dropped a redundant second proto-object encode on push (#421, #416, #544)
- `adopt`/import routed through the streaming pack builder and written as a single pack; `O(entries²)` oplog batch collection made single-pass; `O(n²)` reflog SHA dedup eliminated (#558, #719, #415, #704)
- Native-git engine micro-benchmarks (hashing, compression, pack I/O, tree-diff, delta) and an oplog throughput bench, gated by a weekly scheduled CI workflow (#432, #433, #435)

### Distribution

- Homebrew cask: `brew install --cask heddleco/heddle/heddle` (tap `heddleco/heddle`) — the default install on macOS, with a brew-audit-clean generated cask (#740)
- Signed, notarized universal `Heddle.app` DMG with the embedded FSKit module and version-aware enable UX ships alongside the CLI (#666, #686, #687)
- Prebuilt binaries on [GitHub Releases](https://github.com/HeddleCo/heddle/releases) for macOS (arm64, x86_64), Linux (arm64, x86_64), and Windows (x86_64); the binary release pipeline cosign-keyless signs every artifact with a `SHA256SUMS` manifest (#69)
- Linux builds pinned to a glibc 2.35 floor (Debian 12 / Ubuntu 22.04) for broad portability (#554)
- Workspace crates auto-publish to crates.io on push-to-main (#73)

### sley (native git substrate)

The git substrate behind `git_substrate` reached crates.io 0.0.3 (20 `sley-*`
crates) and closed a large slice of git-parity over this window:

- Shared CLI-layer engines instead of per-command handlers: a parse-options engine + `DateMode`, `setup_revisions`, shared `DiffOptions`, a `strbuf_expand` format substrate, a `grep-source` engine, and an `unpack-trees` tree-merge engine — with `log`, `rev-list`, `branch`, `diff`, `show`, `for-each-ref`, `format-patch`, and others migrated onto them ([sley #51, #61, #56, #64, #71, #84](https://github.com/HeddleCo/sley/pulls))
- Conformance gains against the git oracle: `fsck` (object-content checker), `reset` (ORIG_HEAD, parse-options), `status` (config-aware format + submodule summary), `format-patch` (headers, cover letter, threading), `am`/`apply` (3-way fallback), `rm`, and `update-ref --stdin` ([sley #94, #92, #88, #89, #100, #90, #103](https://github.com/HeddleCo/sley/pulls))
- Read- and write-path perf: mmap commit-graph, loose-object presence cache, buffer reuse, and a once-per-command `.gitattributes` matcher ([sley #98, #79, #101, #78](https://github.com/HeddleCo/sley/pulls))

## 0.2.4 - 2026-05-14

### Added

- **`RedactionTransfer` gRPC message + oneof variants**
  (`crates/grpc/proto/heddle/v1/service.proto`). Out-of-pack
  transport for redaction sidecars on hosted-gRPC push and pull.
  - New `message RedactionTransfer { string blob_hash; bytes redactions_blob; }`.
  - Added as variant 5 to `PushMessage.body` oneof and variant 6 to
    `PullMessage.body` oneof.
  - Sender emits one per blob hash with an active redaction in the
    state closure; receiver routes through
    `Repository::accept_wire_redactions` for signature + trust-list
    verification.

Sidecars live structurally outside `.heddle/objects/` (so GC can't
reach them) and therefore can't ride the native-pack object
channel. This release closes the follow-up flagged in 0.2.3's
"hosted-gRPC redaction transport" note. Server-side biscuit
capability gating + the actual handler wiring lives downstream of
heddle (in `weft-server`), so this release is purely the wire-format
addition.

## 0.2.3 - 2026-05-14

### Added

- **Cross-replica redaction wire propagation** (#12). A signed
  `Redaction` declared on one replica now propagates via the existing
  object-transfer machinery and the receiver replays it: verifies the
  signature, checks the signer against the trust list, persists the
  sidecar, and replays any `purged_at` byte-removal locally.
  - New `wire::ObjectType::Redaction` variant; `enumerate_state_closure`
    emits a Redaction entry for every blob in the closure that has a
    sidecar.
  - New `ObjectStore` trait methods for sidecar access:
    `has_redactions_for_blob`, `get_redactions_bytes_for_blob`,
    `put_redactions_bytes_for_blob`, `list_blobs_with_redactions`.
    Default impls return empty; `FsStore` and `InMemoryStore` implement.
  - New `Repository::accept_wire_redactions`: decodes the incoming
    `RedactionsBlob`, refuses unsigned records, rejects signatures from
    untrusted keys, rejects tampered records, then merges via
    content-addressed idempotency. Records with `purged_at: Some(_)`
    drive a local `purge_blob`.
  - New `WireRejection` enum: `Unsigned` | `Tampered` |
    `UntrustedKey { algorithm, public_key }`. Trust gate is fail-closed:
    an empty trust list rejects every signed redaction.
  - New `[redact] trusted_keys` repo config section. Operators manage
    via the new CLI subcommands:
    - `heddle redact trust add --from-pem <path>` (or
      `--algorithm <a> --public-key <hex>`)
    - `heddle redact trust list`
    - `heddle redact trust remove <hex>`
  - `LocalSync` ferries redaction sidecars during local→local copies.
    Propagation runs unconditionally on every state walk, so the
    redact-after-peer-fetched flow re-syncs correctly even when no new
    objects are copied.
  - `heddle fetch <remote>` no longer short-circuits when the state is
    already present locally; the redaction sweep needs the walk to run.
- **Ignore-hint on `redact`/`purge` output** (#12). After a
  redact/purge, the working-tree file is unchanged — the next
  `heddle capture` would re-snapshot the leaked bytes. The CLI now
  emits a hint pointing at `.heddleignore` if the path isn't already
  covered by heddle's effective ignore set (`.heddleignore` + repo
  config `worktree.ignore`). Coverage check uses gitignore-spec globs.
- **`.heddleignore` upgraded to full gitignore-spec** (#12). `*` and
  `**` globs, character classes (`[abc]`), `!` negation rules, leading
  `/` for root-anchored matches, trailing `/` for directory-only.
  Both matchers (`objects::worktree::should_ignore` and the compiled
  `WorktreeIgnoreMatcher`) delegate to `ignore::gitignore`. Legacy
  patterns behave identically; the three root-admin special-cases
  (`.heddle`, `.heddleignore`, `.git`) stay root-anchored.

### Changed

- `WorktreeIgnoreMatcher::fingerprint` hashes raw pattern strings in
  declaration order (was: sorted). Gitignore semantics are
  order-sensitive — `*.log` then `!keep.log` is not the same as the
  reverse. Cache keys reflect that now.
- `wire::native_pack::build_native_pack` skips `Redaction` entries;
  sidecars live structurally outside `.heddle/objects/` so GC can't
  reach them and they don't enter the content-addressed pack.
- `wire::object_transfer::store_received_object` refuses
  `ObjectType::Redaction` so callers route via
  `Repository::accept_wire_redactions` (forcing signature verification).

### Security

- Closed a spoof vector in cross-replica redaction propagation:
  before this release, `verify_wire_redaction` accepted any
  mathematically-valid signature because it verified using the public
  key embedded in the redaction itself. An attacker could mint a
  redaction, sign with their own key, and pass. The new trust gate
  ties acceptance to an operator-configured `[redact] trusted_keys`
  list. Fail-closed default rejects every signed redaction until the
  operator explicitly trusts a key via `heddle redact trust add`.

## 0.2.2 - 2026-05-14

### Added
- New `heddle-client` crate — the hosted-backend command implementations
  (`auth`, `support`, `presence`), the gRPC client wrappers, and the
  global credential store. Moved out of the closed `weft` workspace and
  into OSS heddle so the workspace builds standalone with
  `--all-features` and third-party hosted clients can extend via the
  public gRPC protocol surface rather than a closed crate.

### Changed
- `heddle-cli`: optional dep `weft-client = "0.0"` → `heddle-client`.
  The feature gating that dep renamed `weft-client` → `client`.
- `heddle-cli-shared`: `UserConfig::weft_client_config` →
  `UserConfig::heddle_client_config`.

### Note
- The `weft-client` placeholder crate on crates.io (v0.0.0) is now
  unreferenced. Safe to yank.

## 0.2.1 - 2026-05-14

### Fixed
- `oplog`: `Repository::op_scope` no longer canonicalizes HEAD to its
  absolute filesystem path. Every recorded op was embedding the user's
  home directory and username (`/Users/<name>/.../.heddle/HEAD`) into
  the oplog. The fix records a blake3 digest of the canonical pointer
  path (`wt-<16-hex>`) — stable per checkout, unique across worktrees
  that share an oplog backend, opaque on disk.

## 0.2.0 - 2026-05-13

First public release on crates.io. The CLI and its workspace ship as
`heddle-*` crates under Apache-2.0. The hosted backend is closed and
links these crates from a separate workspace.

### Added
- Build flavors: `git-overlay`, `native`, both — pick at install time via Cargo features
- `weft-client-shim` trait surface for closed-side hosted-client integration
- TTY-aware CLI flags, error hints, and inline examples on the most-used commands

### Changed
- FsStore reads from packfiles before falling back to loose objects
- `gc` command creates packfiles instead of being a no-op

### Fixed
- `git_import`: `timestamp_opt().unwrap()` replaced with `.single().ok_or_else(...)` — prevents panic on out-of-range Git commit timestamps
- `init`: `current_dir().unwrap()` replaced with proper error propagation — prevents panic when working directory is inaccessible
- `packed_refs`: `save()` now uses a random temp-file suffix to prevent concurrent-write collisions
- `fs_pack`: `pack_objects` now calls `sync_all()` after writing pack and index files for crash durability
- `fs_pack`: `prune_loose_objects` handles `NotFound` errors atomically instead of using a TOCTOU `exists()` check
- `refs_manager`: `delete_track` delegates to `update_refs` for atomic, locked deletion of both loose and packed entries
- `refs_manager`: `set_remote_track` uses `ok_or_else` instead of `unwrap()` on `path.parent()`
- `refs_manager`: `delete_remote_track` ignores `NotFound` errors from `remove_file` to handle concurrent deletes
- `revert`: validates target tree exists before proceeding instead of silently using `unwrap_or_default()`
- `clone`: `copy_worktree` and `copy_dir_recursive` correctly handle symlinks instead of following them
- `State::with_change_id` invalidates cached content hashes when it rewrites logical identity
- `heddle-grpc`: bundles the proto file inside the crate so `cargo publish` ships it
- `heddle-cli`: excludes `tests/realworld_git/fixtures/` and `tests/snapshots/` from the published tarball to stay under the 10 MB crates.io limit
- `heddle-ingest`, `heddle-daemon`: forward `git-overlay` / `native` features through workspace dependencies

### Tests
- 450+ tests passing across the OSS workspace

### Documentation
- README.md: install paths + feature flavors
- AGENTS.md: updated known limitations and status
- docs: 2026-04-14 Rust workspace audit covering docs/code alignment, public surfaces, and verification results
- docs: 2026-05-12 CLI dep audit from dogfooding session

## 2026-05-12 — OSS extraction prep

- Daemon trait extraction, `git-overlay` / `native` mode gating, `weft/` and `tapestry/` renames, SPDX headers across the workspace
- CLI polish for OSS — TTY-aware flags, error hints, examples, docs

## 2026-05-11

- Workflow integration, 17 plan items, 24 Codex review fixes, OSS prep
- build: trim dev-profile debug info for faster incremental links

## 2026-05-09 — Git-overlay foundation

- OSS Heddle CLI: git-overlay foundation, threads, real-world fixtures
  - Native git-overlay replacement workflows
  - Linked-worktree write-through
  - Reflog UX and real-world Git stress fixtures
  - Default `bridge git import` path to current repo
  - Tag-scoped git-overlay history guidance

## 2026-05-05 — Mount and daemon

- feat(mount): virtualized threads, heddled daemon, reflink CoW worktrees
  - `ContentAddressedMount` projects `(StateId, overlay) → POSIX FS` via the `PlatformShell` trait; stateless on the read path
  - Two-tier write model: in-memory buffers → CAS-promoted blobs on flush/release/idle
  - Clock-driven safety sweep promotes idle buffers (default 5s)
  - Heddled daemon (opt-in `--daemon`): mounts stay alive across CLI invocations; shared scaffolding in `crates/repo/src/daemon/`
  - Linux FUSE shell via `fuser`, gated `--features fuse`
  - 39 mount tests including proptest, cross-thread dedup, crash recovery

## 2026-05-01

- perf: pack-batch snapshot blobs (32s → 117ms) + reload-on-miss for multi-instance
- Bridge ingest, provenance, and semantic-hotspots overhaul

## 2026-04-30

- Rename project **\[redacted\] → Heddle**
  - 14 Rust crates renamed: `[redacted]-X` → bare `X` (`[redacted]-core` → `objects`)
  - CLI binary: `[redacted]` → `heddle`
  - Proto package: `[redacted].v1` → `heddle.v1`
  - Workspace `[lib]` name overrides dropped so each crate's library name defaults to its package name

## 2026-04-23

- Claude Code hook integration + ingest matcher/loader fixes

## 2026-04-22

- Add constraint/invariant/rationale annotation taxonomy
  - `AnnotationKind { Constraint, Invariant, Rationale }` shipped end-to-end through object model, proto, gRPC, CLI, and web
  - `--kind` flag on context subcommands (default `rationale`)
  - JSON output renders the kind alongside text

## 2026-04-21

- Workspace control tower + bare `Repository::init` refactor
  - `Repository::init` is now a bare primitive; `init_default` seeds a `main` thread
  - `seed_default_thread` made public; init is no longer undoable
  - `thread_is_unclaimed_bootstrap` lets the git bridge tolerate a seeded main while catching real divergence
- feat(ingest): import git history with agent attribution + reasoning annotations

## 2026-04-18

- Performance improvements

## 2026-04-13

- Improve local performance across status, history, and oplog paths
- Add low-ceremony workflow commands and sync thread metadata

## 2026-04-04

- Remove hosted CLI subcommand from the OSS surface; review pipeline lives in the closed workspace

## 2026-03-24

- Add semantic diff classification, noise filtering, and blast radius analysis
- Refactor: split oversized files, fix clippy warnings, and audit `unwrap` usage

## 2026-03-20

- Make undo lane-local across parallel worktrees
- Refine thread-first CLI and workflow APIs

## 2026-03-15

- feat: semantic rename/move detection in three-way merge
  - Three-signal composite scorer (delta similarity, tree-sitter AST similarity, path heuristics)
  - Rename/rename and rename/delete conflict detection
  - Directory rename inference from grouped file renames
  - CLI output: `R` lines in text, `renames` / `directory_renames` in JSON
  - 22 unit + 9 integration tests

## 2026-03-14

- fix: recursive three-way merge and tree materialization
  - `three_way_merge` recurses into subtrees when both sides have a tree entry at the same name
  - `apply_merged_tree` (merge) and `apply_tree_to_worktree` (cherry-pick) delegate to recursive checkout instead of writing only top-level entries
- chore: clean up feature-gated builds and remove unused `state_pack`

## 2026-03-13

- feat(core): add context annotations for file/symbol/line-range metadata
  - Parallel context tree mirrors the content tree; content-addressed and version-tracked
  - CLI: `context set / get / list / rm`

## 2026-03-12

- feat(core): delta compression, V1/V2 pack format, and bridge migration tests
  - Path-aware delta compression with sliding window base selection
  - Git-style copy/insert delta encoding
  - V2 varint-encoded pack format with raw zstd compression
  - Auto-pack on import; improved pack reader and base selection
- refactor: extract `core` and thin `cli` / `server`

## 2026-03-11

- refactor: extract `refs` and `oplog` into workspace crates
- feat: replace the custom wire protocol with gRPC transport

## 2026-03-09

- refactor: rename `tracks` → `threads` across the codebase

## 2026-03-07

- Finish pure-Rust git transport merge — gitoxide bridge and fetch integrated; end-to-end transport and CLI coverage added

## 2026-02-20 — Multi-agent worktrees

- feat: multi-agent parallel worktrees + security/correctness fixes
  - `Repository::open` detects `.heddle` as a file (object-store pointer) for filesystem-isolated agent checkouts that share one store
  - `RefManager::with_local_head` gives each checkout its own HEAD file so concurrent agents on different threads don't contend on HEAD
  - `worktree add <path> [--track] [--from]` materializes a state into a new isolated directory with a `.heddle` pointer file
  - Lightweight agent session registry stored as TOML under `.heddle/agents/`
  - 22 multi-agent worktree integration tests
- fix: symlink validation bypass, unhandled symlinks in status, non-atomic oplog writes
  - Replace `canonicalize` fallback with lexical normalization to prevent path traversal via dangling symlinks
  - Atomic-write pattern (temp file + `sync_all` + rename) for oplog writes

## 2026-02-19

- refactor: split all oversized files to comply with the 300-line limit; split `refs_transactions` below 300 lines
- fix: packed-refs CAS bug
- fix: 14 panics, races, and durability issues from internal code review

## 2026-02-17 — v0.1.1 (internal milestone)

- feat: extensible cryptographic state signing trait (Ed25519, RSA, P-256)
- feat: shallow clone support with `--depth N`
- feat: packfiles for efficient storage
- feat: hooks system for repository lifecycle events (`hook list/install` for pre/post snapshot/merge/rebase)
- feat: `bridge git sync` for bidirectional git-heddle synchronization
- feat: `file://` protocol support for local repository sync
- feat: proper blame and rebase with commit replay (`--abort`, `--continue`)
- feat: bisect session validation (requires `bisect start` first); `good` / `bad` accept optional commit defaulting to HEAD
- feat: clean, revert, stash, and merge commands
- feat: cherry-pick command for applying specific commits
- feat: fsck command for repository integrity verification
- feat: fetch and clone commands for remote repository bootstrapping
- feat: resolve command with `--ours`, `--theirs`, `--all`, `--abort`
- feat: completion command for shell completion scripts
- feat: tracing instrumentation for snapshot, tree, and store operations
- feat: merge-state tracking in `.heddle/MERGE_STATE`
- Tests: 395+ tests passing, 2 ignored (file:// protocol, macOS permissions)

## 2026-02-09

- Add `.heddleignore` support and unified ignore handling
- Improve CLI test coverage and state-spec resolution
- Add Apache-2.0 license and NOTICE metadata

## 2026-02-02 — Foundation

Initial implementation of an AI-native version control system, landed
as five phases over a single development sprint.

### Phase 1 — Foundation

- Initial project structure
- Object model: `Blob`, `Tree`, `State`, `Action`
- Basic CLI with clap
- Filesystem storage implementation
- Configuration management

### Phase 2 — Core VCS

- Content-addressed storage with BLAKE3
- Change IDs for stable identifiers
- Tracks and markers for branching
- Operation log with undo/redo
- Worktree management
- `snapshot`, `log`, `show`, `goto`
- `diff` and `compare`
- `fork` and `collapse`

### Phase 3 — Semantic diff

- Tree-sitter based code parsing
- Function-level change detection
- Rename detection across files
- Import/export analysis
- Semantic diff output format

### Phase 4 — Wire protocol primitives

- Length-delimited framing for the wire protocol
- MessagePack serialization for all protocol messages
- Capability negotiation between client and server
- Reference advertisement (`ListRefs` / `RefsList`)
- Object-transfer message shape with state-closure computation
- The `wire` and `grpc` crates publish as `heddle-wire` and `heddle-grpc`; consumers build their own client/server on top

### Phase 5 — Git bridge

- Bidirectional git-heddle synchronization
- Pure-Rust git transport via gitoxide
