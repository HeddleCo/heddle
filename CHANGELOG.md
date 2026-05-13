# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog, and this project follows calendar versioning for entries.

## Unreleased

### Added
- `heddle clone --depth N`: Shallow clone support for partial history downloads
- Packfiles with delta/zstd compression via `heddle gc --aggressive/--prune` (50-70% space savings)
- Hooks system with `heddle hook list/install` for pre/post snapshot/push/pull/merge/rebase
- `heddle bridge git sync` for bidirectional git-heddle synchronization
- `file://` protocol for local repository sync without network overhead
- Extensible cryptographic state signing trait (Ed25519, RSA, P-256) via `heddle capture --sign key.pem`

### Changed
- FsStore now reads from packfiles before falling back to loose objects
- GC command creates packfiles instead of being a no-op

### Fixed
- `hosted serve`: object store honors the `s3` feature when an `[s3]` config block (or `HEDDLE_SERVER_S3_*` env vars) is set — previously always wrote blobs to the local filesystem regardless of feature flags
- `git_import`: `timestamp_opt().unwrap()` replaced with `.single().ok_or_else(...)` — prevents panic on out-of-range Git commit timestamps
- `init`: `current_dir().unwrap()` replaced with proper error propagation — prevents panic when working directory is inaccessible
- `packed_refs`: `save()` now uses a random temp-file suffix to prevent concurrent-write collisions
- `fs_pack`: `pack_objects` now calls `sync_all()` after writing pack and index files for crash durability
- `fs_pack`: `prune_loose_objects` now handles `NotFound` errors atomically instead of using a TOCTOU `exists()` check
- `refs_manager`: `delete_track` now delegates to `update_refs` for atomic, locked deletion of both loose and packed entries
- `refs_manager`: `set_remote_track` uses `ok_or_else` instead of `unwrap()` on `path.parent()`
- `refs_manager`: `delete_remote_track` ignores `NotFound` errors from `remove_file` to handle concurrent deletes
- `revert`: validates target tree exists before proceeding instead of silently using `unwrap_or_default()`
- `clone`: `copy_worktree` and `copy_dir_recursive` now correctly handle symlinks instead of following them
- Hosted thread summaries now prefer live ref state over stale stored metadata after captures and other ref updates
- `State::with_change_id` now invalidates cached content hashes when it rewrites logical identity

### Tests
- 450+ tests passing
- Added regression coverage for hosted thread-summary overlays and logical-ID hash invalidation

### Documentation
- README.md: Updated feature list and test count
- AGENTS.md: Updated known limitations and status
- docs: Added 2026-04-14 Rust workspace audit covering docs/code alignment, public surfaces, and verification results
- docs: Corrected hosted admin and partial-fetch status language to match the current gRPC-first and foundation-level implementation

## 2026-02-17

- Added: Rebase command with commit replay, `--abort`, and `--continue` support
- Added: Blame command now extracts author/timestamp from state metadata
- Added: Bisect session validation (requires `heddle bisect start` first)
- Added: Bisect `good`/`bad` commands now accept optional commit (defaults to HEAD)
- Fixed: Blame test assertions for line attribution
- Fixed: Rebase state persistence using full ChangeId encoding
- Tests: All 395+ tests passing, 2 ignored (file:// protocol, macOS permissions)

## 2026-02-16

- Added: Comprehensive test suite (59 tests for core functionality)
- Added: Production feature tests (36 tests for VCS commands)
- Added: Tracing instrumentation for snapshot, tree, and store operations
- Added: Resolve command with --ours, --theirs, --all, --abort options
- Added: Fsck command for repository integrity verification
- Added: Fetch command for remote object download
- Added: Clone command for remote repository bootstrapping
- Added: Cherry-pick command for applying specific commits
- Added: Gc command for garbage collection
- Added: Bisect command for binary search
- Added: Blame command for line attribution
- Added: Completion command for shell completion scripts
- Added: Merge state tracking in `.heddle/MERGE_STATE`

## 2026-02-09

- Added: .heddleignore support and unified ignore handling
- Improved: CLI test coverage and state spec resolution
- Added: License and NOTICE metadata

## Phase 4 (Wire Protocol)

- Added: TCP-based wire protocol with length-delimited framing
- Added: MessagePack serialization for all protocol messages
- Added: Capability negotiation between client and server
- Added: Token-based authentication
- Added: Push/pull operations for remote sync
- Added: Reference advertisement (ListRefs/RefsList)
- Added: Object transfer with state closure computation
- Added: Remote management commands (add, list, remove)
- Added: Server mode with `heddle serve`
- Documentation: PHASE4_SUMMARY.md with protocol details

## Phase 3 (Semantic Diff)

- Added: Tree-sitter based code parsing
- Added: Function-level change detection
- Added: Rename detection across files
- Added: Import/export analysis
- Added: Semantic diff output format

## Phase 2 (Core VCS)

- Added: Content-addressed storage with BLAKE3
- Added: Change IDs for stable identifiers
- Added: Tracks and markers for branching
- Added: Operation log with undo/redo
- Added: Worktree management
- Added: Snapshot, log, show, goto commands
- Added: Diff and compare commands
- Added: Fork and collapse commands

## Phase 1 (Foundation)

- Initial project structure
- Object model (Blob, Tree, State, Action)
- Basic CLI with clap
- Filesystem storage implementation
- Configuration management
