# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog. Public releases follow semver
(starting with `0.2.0`); pre-release entries below the public release are
grouped by date to capture the internal development history.

Only changes that affect the OSS CLI and its supporting crates are
recorded here. Hosted-product work (Postgres, Biscuit, the web app,
GitHub App, etc.) lives in the closed `HeddleCo/weft` and
`HeddleCo/tapestry` repos.

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
- The `proto` and `grpc` crates publish as `heddle-proto` and `heddle-grpc`; consumers build their own client/server on top

### Phase 5 — Git bridge

- Bidirectional git-heddle synchronization
- Pure-Rust git transport via gitoxide
