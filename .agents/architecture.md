# Architecture Guidelines

## Directory Structure

The repository is now a Cargo workspace. Do not assume a single `src/` tree is the whole system.

```text
crates/
  cli/        # CLI entry point, args, command dispatch
  objects/       # core object model and shared types
  repo/       # repository operations and helpers
  refs/       # threads, markers, HEAD, packed refs
  oplog/      # undo/redo oplog logic
  cli/src/bridge/    # Git interoperability (module within the cli crate)
  semantic/   # semantic diff and parser-heavy analysis
  ...
docs/              # architecture, hosted model, roadmap, future-state plans
specs/             # Quint formal specifications
tests/             # integration and property tests
```

The hosted server now lives in the sibling **weft** repo and the SvelteKit web
product in the sibling **tapestry** repo — neither is part of this workspace.

## Key Design Patterns

### 1. Repository as Central Coordinator

```rust
pub struct Repository<R = RefManager, O = OpLog, S = AnyStore> {
    root: PathBuf,             // working directory (checkout root)
    heddle_dir: PathBuf,         // shared .heddle directory (may differ from root/.heddle in agent checkouts)
    store: S,                  // object store backend; AnyStore = FsStore | S3Store (static dispatch)
    refs: R,                   // threads, markers, HEAD (HEAD may be per-checkout)
    oplog: O,
    config: Config,
    shallow: ShallowInfo,
}
```

The Repository type coordinates between all subsystems. Most operations use it as the primary interface.
The backends are type parameters (heddle#259 / #283) so the CLI monomorphizes to the on-disk
local flavor — the bare name `Repository` resolves to `Repository<RefManager, OpLog, AnyStore>` — while
the hosted server can swap in Postgres-backed ref/oplog backends. `AnyStore` is an enum over the
concrete object stores, so the `FsStore`-vs-`S3Store` choice stays a runtime decision without a vtable.

In a standard repo `heddle_dir == root/.heddle`. In an agent checkout `root` is the checkout
directory and `heddle_dir` is the *shared* `.heddle` from the main repo — both are set by
`Repository::open()` when it detects a `.heddle` pointer file.

### 2. Trait-Based Storage Abstraction

```rust
pub trait ObjectStore {
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>>;
    fn put_blob(&self, blob: &Blob) -> Result<ContentHash>;
    // ...
}
```

Enables testing with in-memory implementations and future alternative backends (S3, database, etc.).

### 3. Command Pattern for CLI

```rust
pub fn execute(cli: &Cli, repo: &Repository) -> Result<()> {
    match &cli.command {
        Commands::Init { .. } => cmd_init(..),
        Commands::Status { .. } => cmd_status(..),
        // ...
    }
}
```

### 4. Content-Addressed Storage and Immutability Model

All objects are immutable and content-addressed, enabling:
- Automatic deduplication
- Safe sharing between states
- Verifiable integrity

**Immutability is at the object level, not the history level.** State objects, once stored, are
never mutated or deleted. However, threads (mutable named pointers) can be moved to any state,
including non-ancestors. Operations like `rebase`, `collapse`, and `cherry-pick` create **new**
state objects with different parents — the originals remain in the store permanently. Force push
(`push --force`) is supported and required after history-rewriting operations since the thread
pointer is non-fast-forward. The key guarantee: no data is ever lost, even after a rebase or
force push.

### 5. Agent Checkout Pattern

A directory becomes an agent checkout by placing a `.heddle` file (not directory) containing:
```
objectstore: /abs/path/to/shared/.heddle
```

`Repository::open()` detects this and opens the shared `FsStore` while treating the local directory as the working root. Each checkout has its own `HEAD` file alongside `.heddle`, managed via `RefManager::with_local_head()`. This provides full filesystem isolation per agent with zero object duplication.

The `.heddle/agents/` directory in the shared store holds lightweight TOML session records linked to threads for orchestration and attribution.

### 6. Hosted / Stateless Server Pattern

Hosted deployments now have two distinct layers:

- **data plane** - Heddle protocol server, repository object access, refs, oplog
- **control plane** - hosted namespaces, repositories, grants, admission, admin APIs

Important current direction:

- Postgres is the durable metadata source for hosted control-plane state.
- S3-compatible storage is the shared object backend for hosted repositories.
- Ephemeral coordination should use an external backend in hosted mode, not in-process maps.
- The server still carries some sync-over-async bridges in Postgres-backed paths; removing those remains a priority for scale work.

### 7. Web App (SvelteKit)

The SvelteKit web product (marketing site + hosted app prototype) lives in the
sibling **tapestry** repo, not in this workspace. Its repo content pages use
`+page.server.ts` loaders so API credentials never reach the browser. Some
authenticated surfaces are fully wired to the backend, while others are still
mock-backed or partial. Consult the tapestry repo's product spec and route
implementations before treating a web surface as shipped behavior.

### 8. Packfile and Delta Compression

Objects are stored in packfiles with varint-encoded sizes and
Git-style delta compression. The pack builder uses a sliding window (W=10) to
find optimal delta bases, with objects sorted by extension then basename for
best adjacency. See `.agents/delta-compression.md` for full details on the
algorithm, format, strategies tried, and future improvement opportunities.

Key: the `zstd` feature flag is not a default feature. Pack/delta
tests require `cargo test --features zstd` for full coverage.

### 9. Multi-Agent Undo Semantics

Undo/redo is scoped to the current checkout thread. Oplog entries carry the
checkout's HEAD-path scope, and undo/redo selects batches only from that scope.
Shared refs still live in the shared store, but batch selection is no longer
repository-global.

### 10. Thread + Actor + Session Model

When working on harness or orchestration features, use this model:

- `thread` = human-facing Heddle work context
- `actor` = active worker on that thread
- `session` = execution/provenance record for that actor
- `segment` = provider/model epoch inside that session

Important current direction:

- Heddle should follow harnesses ambiently instead of making users run tools through Heddle
- `heddle harness-bridge` is an internal protocol surface, not the main product abstraction
- strong harness-native keys should drive actor identity before path/thread heuristics
- if Heddle is uncertain, prefer creating a new actor/session over ambiguous reuse

The local actor registry lives in `.heddle/agents/`. Local session reports live in `.heddle/state/session-reports/`.

### 11. Formal Specifications (Quint)

Core state machines are formally specified in `specs/quint/` using [Quint](https://quint-lang.org/). Each spec defines state variables, guarded actions, safety invariants, and regression traces. Corresponding Rust property tests in `tests/formal_specs.rs` mirror the specs using `proptest`.

Specs exist for: merge/conflict resolution, lock protocol, refs/HEAD with CAS, agent lifecycle, worktree lifecycle, and composed repository operations. See `.agents/formal-specs.md` for when and how to update them.
