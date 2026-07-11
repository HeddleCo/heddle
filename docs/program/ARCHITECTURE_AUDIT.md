# Architecture audit (baseline)

Recorded against commit in `artifacts/baseline/commit.txt`.

## Workspace map and dependency direction

```text
heddle-cli (apex binary)
  ├─ heddle-core          # embeddable facade (partial extraction)
  ├─ heddle-cli-shared    # UserConfig, remotes, logging/output shared types
  ├─ heddle-client        # optional hosted client
  ├─ heddle-daemon        # local gRPC/UDS services
  ├─ heddle-git-projection
  ├─ heddle-ingest
  ├─ heddle-mount (opt)
  └─ domain: repo, objects, refs, oplog, merge, semantic, wire, crypto, format, …

heddle-core
  └─ repo, objects, merge, semantic?, refs, oplog, git-projection, crypto, cli-shared

Domain crates must not depend on heddle-cli or clap/anstyle (enforced by
scripts/check-facade-render-free.sh). Graph is acyclic with CLI at the apex.
```

Approximate Rust LOC (source + tests under each crate):

| Crate | ~LOC | Role |
|-------|------|------|
| cli | 201k | Delivery + large residual domain |
| repo | 54k | Repository coordinator |
| objects | 26k | Object model / store |
| mount | 21k | FUSE/ProjFS/FSKit |
| semantic | 20k | AST diff/merge |
| core | 17k | Facade (status/verify/diff/merge/save…) |
| ingest | 14k | Git import walk/translate |
| client | 13k | Hosted client |
| daemon | 10k | Local agent services |
| git-projection | 9k | Git projection engine |

## Layer ownership problems

### 1. Delivery layer still owns domain semantics

Largest residual command modules (lines):

- `command_catalog/mod.rs` ~4.9k
- `schemas.rs` ~3.8k
- `thread.rs` ~3.7k
- `clone.rs` ~2.5k
- `status.rs` ~2.4k (partially dual with `heddle_core::status`)
- `start_atomic.rs` ~2.2k
- `undo_apply` ~2.2k+
- `remote/*`, `workflow.rs`, `agent_cmd.rs`, `integration.rs`

Target: CLI owns parse/dispatch/render/exit only; ops live in `heddle-core` or domain crates.

### 2. Monolithic facade modules

`heddle-core` already holds:

- `status.rs` ~3.1k
- `merge/mod.rs` ~3.2k
- `verify.rs` ~1.7k

These are better than CLI fusion but still large; further splits should follow cohesive interfaces (not arbitrary line caps).

### 3. Global / ambient state

| Site | Risk |
|------|------|
| CLI `USER_CONFIG` OnceLock | Blocks multi-repo-in-process |
| CLI color `COLOR_STATE` | Process-global |
| `repo::lazy_hydrator` REGISTRY | Process-global hydrator factories |
| `objects` fault_inject FAULT_POINTS | Test/prod ambient (intentional for fault harness) |
| `semantic` AST CACHE | Process-global cache |
| thread_local caches in snapshot/visibility | Acceptable if not cross-repo poisoned |

ADR 0040 marks de-singleton of faults + semantic cache as future work on `ExecutionContext`.

### 4. Process control / side channels

Production `process::exit` remains in CLI (`main`, op-id replay, merge/operator, snapshot disk-full, try child passthrough). Domain crates largely return `Result`. Residual `eprintln!` in refs lock Drop and packed oplog recovery.

### 5. External process dependencies

| Dependency | Contract status |
|------------|-----------------|
| `git` executable | **Must not** be required for public overlay workflows; linted. Tests may use `git`. |
| Sley | **In-contract** native Git-format engine |
| `watchman` | Optional fsmonitor acceleration (`repo::fsmonitor`) — not part of core contract |
| Browser open helpers | Auth login only (`open` / `xdg-open` / `cmd`) |
| FUSE/mount platform tools | Optional mount feature |

Gap: `git_process_lint` source dirs omit `crates/core` and `crates/git-projection` (currently test-only git spawns, but lint should cover them).

### 6. Duplicate repository / context setup

- `Cli::open_repo()` ambient open vs `ExecutionContext` builder with injected `Repository`
- Status/verify paths partially migrated to core; many commands still open repo ad hoc
- Git projection / ingest each construct Sley repos independently (expected, but status paths re-open often — perf hotspot)

### 7. Business logic in transport handlers

Local daemon gRPC impls should call facade/domain ops, not reimplement status/merge. Hosted path is weft; keep OSS daemon thin.

## Preferred target shape

```text
CLI / daemon / tests
  -> application session facade (heddle-core::ExecutionContext)
    -> typed domain operations (*Options/*Request/*Plan/*Outcome/*Error)
      -> storage / Sley / wire / clock / fs services (injected)
```

Standardize new ops on typed structures; inject clocks, FS, env, networking, process execution where testability requires it. Avoid giant god-context objects.

## Architecture release criteria (program)

- New features land in domain or `heddle-core`, not as CLI-only semantics.
- CLI handlers that grow domain branches are rejected in review.
- Facade/domain stay render-free (CI gate).
- Runtime git process allowlist stays empty (or explicitly justified + reviewed).
- No new process-global mutable state without ExecutionContext injection plan.
