# Heddle product contract (program baseline)

Status: working contract for the correctness / architecture / performance program.
Grounded in `README.md`, `CONTEXT.md`, `docs/ARCHITECTURE.md`, `docs/adr/0040-embeddable-facade.md`, `docs/PRINCIPLES.md`, `docs/STABILITY.md`, oracle tests, and current code on commit recorded in `artifacts/baseline/`.

## What Heddle is

Heddle is a **local-first, agent-native version control system** shipped as the OSS CLI `heddle` (`heddle-cli`). It keeps its own content-addressed state model and writes Git-compatible state through a checkout’s real `.git` (Git Overlay). Hosted backend (`weft`) and web product (`tapestry`) are separate repositories.

## Intended users and embedding model

| User | Primary surface |
|------|-----------------|
| Human developers | CLI: `status`, `adopt`, `commit`, `start`, `land`, `verify`, `diff`, `log` |
| Coding agents / harnesses | Machine-readable JSON (`--output json` / auto), command catalog, op-id replay, harness integrations |
| Embedders (library) | `heddle-core` facade: typed ops returning `*Report` / `Result`, no process control or render |
| Hosted products | gRPC protos (`heddle-grpc`), `heddle-client`, wire protocol — consumers in weft/tapestry |

Embedding target (ADR 0040):

```text
delivery (CLI / future daemon / tests)
  -> heddle-core (ExecutionContext + typed operations)
    -> domain crates (repo, objects, merge, semantic, refs, oplog, ingest, git-projection, …)
      -> storage / Sley / protocol services
```

## Authoritative behavioral contract

1. **Local-first** — useful without a hosted account; no required network for core VCS.
2. **Agent-native** — durable threads, attribution (principal + agent), retryable ops (`--op-id`), disposable attempts (`try`, isolated `start --path`).
3. **Git Overlay** — active Git reads/writes use the checkout’s real `.git`; Heddle metadata lives under `.heddle`.
4. **Byte-identical Git round-trip** — for public history, adopt/import → export reproduces identical commit/tree/blob/tag SHAs and `git fsck --full` clean (oracle: `roundtrip_fidelity`, git projection engine tests).
5. **No runtime `git` executable dependency** for public Git-overlay workflows — Git-format identity and operations via **Sley** (native engine). Tests/fixtures may shell out to `git`.
6. **Verification-first CLI** — Repository Verification State drives status/doctor/verify advice; mutating commands fail closed when verification is degraded/blocked.
7. **Machine contracts** — stable JSON fields, explicit nulls, command catalog + schemas as source of truth (`docs/json-schemas.md`, doctor schemas gate).
8. **Compatibility posture** — pre-1.0: prefer current model over legacy shims unless explicitly requested.

## Oracles and compatibility targets

| Oracle | What it proves | Location |
|--------|----------------|----------|
| Git object SHA fidelity | Byte-identical import/export for public repos | `crates/cli/tests/roundtrip_fidelity.rs` (+ fixtures) |
| Commit conformance | Git commit object shape vs real corpus | `crates/cli/tests/commit_conformance.rs` |
| Diff/patch conformance | Patch text vs git-shaped expectations | `cli_integration/diff_patch_conformance.rs` |
| Git overlay matrices | Interop / replacement / remote ref behavior | `cli_integration/git_overlay_*.rs` |
| Real-world shaped fixtures | Larger gitoxide/ripgrep/tokio-shaped trees | `cli_integration/realworld_git.rs` + `realworld_git/fixtures/` |
| Formal specs | Model-checked repository/ref/merge properties | `specs/quint/` + `formal_specs` tests |
| Sley (not gix) | Engine dependency is Sley | `git_process_lint` + Cargo manifests |
| Facade render-free | Domain/facade crates lack CLI render deps | `scripts/check-facade-render-free.sh` |
| Process-spawn lint | No unreviewed runtime `git` spawns | `crates/cli/tests/git_process_lint.rs` |

**External reference for performance:** Git (human workflows) and Sley (Git-format engine). Heddle does not claim to replace Git’s full porcelain surface; it claims native Git-format fidelity without a `git` process on the hot path.

### Surfaces that must be byte-identical or semantically equivalent

| Surface | Contract |
|---------|----------|
| Git object bytes (public, lossless path) | Byte-identical SHAs on round-trip |
| Raw Git Object Residuals | Preserve non-reconstructable object bytes |
| ContentHash (BLAKE3) | Content-addressed equality |
| ChangeId (`hd-…` physical) | Stable handle for a specific state, not rewrite lineage |
| Public JSON schemas | Field-stable; alpha may break names when model improves (documented) |
| Wire/gRPC `heddle.v1` | Hosted contract; versioned package, not yet frozen at 1.0 |
| Text CLI | User-facing; not byte-stable |

## Public surfaces

| Layer | Ownership |
|-------|-----------|
| CLI binary `heddle` | `crates/cli` — parse, env/TTY, auth boundary, dispatch, render, exit codes |
| Library facade | `crates/core` (`heddle-core`) — `ExecutionContext`, status/verify/diff/merge/save/query/fsck/thread_shaping |
| Domain | `repo`, `objects`, `refs`, `oplog`, `merge`, `semantic`, `ingest`, `git-projection`, `format`, `crypto`, … |
| Protocol | `wire`, `grpc` protos, `client`, `daemon` (local UDS services) |
| Persistence | `.heddle/` object store, refs, oplog, config; Git Overlay uses real `.git` via Sley |
| Extensions | Harness integrations, mount (FUSE/ProjFS/FSKit), optional semantic languages |

## Supported platforms / features (as shipped)

- **Language / toolchain:** Rust 2024 edition; repo pins current stable (CI + local 1.97+ observed).
- **OS:** macOS (primary dogfood), Linux (CI), Windows (partial — mount ProjFS, materialization helpers).
- **Default CLI features:** `git-overlay`, `native`, `local`, `semantic`, `zstd` (see README install).
- **Optional:** `client` (hosted), `mount` (FUSE worker / platform mounts), extra semantic languages.
- **Databases:** local store is filesystem + optional SQLite (bundled rusqlite); Postgres is hosted (weft), not required for OSS CLI.
- **Runtime:** Tokio where async is needed (daemon, client, some CLI paths).

## Explicit non-goals

- Being a full Git porcelain reimplementation of every `git` subcommand.
- Requiring a hosted account or always-on network for local VCS.
- Last-write-wins collaboration on source history (source history is immutable; collaboration is a separate log).
- Persistent Bridge Mirror (`.heddle/git`) as the active Git store (retired direction; overlay uses real `.git`).
- 1.0 API freeze until `docs/STABILITY.md` thresholds are set and met (currently strawman / TBD).
- Partial clone / lazy object fetch (planned).
- Hosted builds/workflows/artifacts in this OSS repo (planned / weft).

## Known documentation vs implementation tensions

| Claim | Reality to reconcile |
|-------|----------------------|
| `heddle-core` is the embeddable facade | Partial: status/verify/diff/merge/save/query/fsck/thread_shaping extracted; large CLI command surface still owns domain logic (`thread`, `clone`, `workflow`, remotes, undo, …) |
| No runtime git process | Enforced for scanned runtime dirs; `crates/core` not in git-process lint dirs yet; optional `watchman` spawn exists for fsmonitor |
| CLI is thin clap→facade→render | Not yet: `crates/cli` ~200k LOC; many `cmd_*` handlers still fuse compute + render |
| Stability 1.0 gates | `docs/STABILITY.md` still has `<TBD: maintainer>` thresholds |
| ARCHITECTURE.md “hosted namespaces in this codebase” | Hosted server moved to weft; this repo keeps client/proto foundation |
| `docs/STABILITY.md` version numbers | Document cites 0.2.x; workspace package version is `0.10.0` |
