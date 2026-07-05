# Heddle Architecture

This document describes Heddle's current architecture at a high level. It focuses on the system as it exists today, while noting the hosted and web directions already supported by the codebase.

## System Model

Heddle is an agent-native version control system built around three core ideas:

1. **Content-addressed storage** - immutable objects addressed by BLAKE3 content hash
2. **Stable change identity** - logical changes keep stable identifiers even when history is rewritten
3. **Explicit attribution** - changes can carry both human and agent attribution, plus related verification metadata

## Capability Status

### Shipped

- local repository model with immutable states
- threads, markers, refs, oplog, and working tree management
- remote sync and Git projection
- provenance-backed local blame and rewrite preservation
- semantic diff support
- multi-agent worktrees and agent registry
- Heddle-native actor tracking for supported coding harnesses
- local session reporting and harness integration install flows
- hosted namespaces, repositories, grants, and content/admin APIs in foundation form

### Foundation in place

- Postgres-backed hosted control-plane direction
- shared object storage direction for hosted repositories
- web product for repository inspection and operations
- context annotations as a first-class repository concept
- hosted provenance APIs and read-only provenance evidence in file/change inspection
- queued hosted delivery path for local harness session reports

### Planned

- provenance-aware compare/review/history UX beyond current hosted inspection
- hosted builds, workflows, logs, artifacts, and verification writeback
- richer compare/review/history surfaces in the web product
- partial fetch and lazy object retrieval

## Architecture Layers

```text
CLI / Web UI
  -> repository operations and hosted APIs
  -> refs, oplog, worktree, semantic, bridge, authz
  -> object store and metadata backends
  -> immutable objects and hosted control-plane metadata
```

Heddle is no longer best understood as a single `src/` tree. The repository is a Cargo workspace with separate crates for the local/client CLI, core types, repository helpers, refs, oplog, semantic analysis, and bridge functionality. (The hosted server and admin binary moved to the sibling **weft** repo — see below.)

## Workspace Structure

```text
crates/
  cli/       # local CLI entry point, args, command dispatch
  objects/      # core object model and shared types
  repo/      # repository operations and helpers
  refs/      # threads, markers, HEAD, packed refs
  oplog/     # undo/redo oplog logic
  cli/src/bridge/   # Git interoperability (module within the cli crate)
  semantic/  # semantic diff and parser-heavy analysis
  ...
docs/             # architecture, hosted model, roadmap, future-state plans
specs/            # Quint formal specifications
tests/            # integration and property tests
```

The hosted server now lives in the sibling **weft** repo and the SvelteKit web
product in the sibling **tapestry** repo — neither is part of this workspace.

## Configuration Boundaries

Heddle uses separate config/state scopes instead of a single repository config file doing everything:

- `UserConfig` lives in the user's config directory and owns identity, agent defaults, output preferences, and client auth profiles
- `RepoConfig` lives in `.heddle/config.toml` and owns repository-local behavior, storage coordinates, and remotes
- `ServerConfig` lives with the hosted runtime (in the sibling **weft** repo) and owns storage, database, auth, TLS, and admission settings
- `WorktreeState` is checkout-local runtime state and should not be serialized into repo config

Binary ownership follows the same split:

- `heddle` (this repo) owns local repository operations and hosted client access
- the hosted server runtime and admin operations live in the sibling **weft** repo (no `hosted` binary in this workspace)

## Core Repository Model

### Objects

Heddle stores immutable objects such as:

- `Blob` - file content
- `Tree` - directory structure
- `State` - repository snapshot and metadata
- `Action` - operation record linked to state transitions

Important identifiers:

- `ContentHash` - content-addressed object identity
- `ChangeId` - stable logical change identity

### References

Heddle uses:

- `Thread` - mutable named reference to a state
- `Marker` - immutable named reference to a state
- `HEAD` - current checkout pointer

Loose refs take precedence over packed refs. Packed refs exist for scale, while active refs remain loose for fast writes.

### Repository Coordinator

The `Repository` type coordinates the main local subsystems:

- object storage
- refs
- oplog
- working tree materialization
- config
- repository-scoped operations like snapshot, merge, diff, and checkout

In a standard repository, `root` and `heddle_dir` refer to the same checkout. In an isolated worktree, the checkout root is separate from the shared `.heddle` object store.

## Storage Layout

Typical local repository layout:

```text
.heddle/
  objects/
    blobs/
    trees/
    states/
    packs/
  refs/
    threads/
    markers/
    packed-refs
    HEAD
  agents/
  oplog/
  config.toml
  ignore
```

The `config.toml` shown above is repository-local config, not user config or server config. Worktree runtime state is tracked separately from repository config.

Object lookup order is loose object first, then packfile fallback.

## Worktrees And Agent Isolation

An isolated worktree is a normal checkout directory that points at a shared object store via a `.heddle` file.

```text
/workspace/agent-a/
  .heddle   # objectstore: /main/repo/.heddle
  HEAD    # per-checkout HEAD
```

Important current behavior:

- `heddle start <thread> --path <dir>` creates a filesystem-isolated checkout
- `heddle start <thread>` records thread and agent metadata while keeping execution roots private by default
- the oplog is still global across worktrees, so undo/redo semantics are repository-global rather than checkout-local

## Threads, Actors, And Sessions

Heddle's current coordination model is best understood as:

- **thread** - the human-facing work context
- **actor** - the active worker on that thread
- **session** - the execution and provenance record for that actor
- **segment** - a provider/model epoch within a session

This matters for harness integration work:

- Heddle should follow supported harnesses ambiently rather than requiring users to run tools through Heddle
- actors are stored in the lightweight registry under `.heddle/agents/`
- harness-native identities are tracked separately from Heddle-local reconnect identifiers
- `heddle actor ...` is the user-facing inspection surface over that registry

The current harness integration logic lives in `cli`, not in `objects` or `repo`, because it is still CLI/runtime orchestration rather than a stable repository primitive.

See [HARNESS_ACTOR_INTEGRATION.md](./HARNESS_ACTOR_INTEGRATION.md) for the detailed model and attach rules.

## Harness Integration And Local Reporting

Heddle currently supports ambient-follow integration for three harnesses:

- `codex`
- `claude-code`
- `opencode`

Current shipped behavior:

- installed hooks/plugins relay into that bridge through an internal `heddle integration relay ...` command
- session reports are persisted locally under `.heddle/state/session-reports/`
- actor attach is probe-first and based on harness-native keys when available
- OpenCode installs a Heddle-owned plugin and a `heddle.timeline.json`
  capability manifest. The plugin relays OpenCode events; the manifest
  advertises shipped timeline navigation commands (`log --timeline`,
  `timeline fork`, `timeline reset`, `timeline recover`) for agents and
  desktop integrations without relying on an unverified native OpenCode tool
  registration API.

Current install flow:

- `heddle init` can optionally offer harness integration install in interactive mode
- `heddle integration install/list/doctor/upgrade/uninstall` manage Heddle-owned hook/plugin setup

Important design direction:

- Heddle should not become a general “run commands through me” wrapper
- if attach is ambiguous, Heddle should prefer creating a new actor/session over reusing one incorrectly

## Context Annotations

Context annotations are a core Heddle concept. They attach revisable rule, why, gotcha, and migration guidance to file paths, symbols, line ranges, or broader states and travel with snapshot history.

### Current model

- context is stored as a parallel content-addressed tree rooted from `State.context`
- logical annotations have stable IDs, revision history, and supersede relationships
- current revisions are part of immutable snapshot history, while newer revisions are captured in later states
- `heddle context get`, `set`, `list`, `history`, `edit`, `supersede`, `suggest`, `audit`, and `rm` operate on that model
- agent sessions may log context queries for later inspection

### Product direction

This model supports Heddle's larger review and repository-intelligence story: code can already be inspected with scoped guidance, provenance, and attribution in one surface, and it should continue expanding toward richer compare/review and verification workflows.

## Semantic Analysis

The semantic layer provides code-aware diff and structural inspection. It is intentionally helpful rather than magical: semantic output exists today, but can still be conservative.

## Git Bridge And Remote Sync

Heddle supports:

- remote synchronization over its own protocol
- import from Git
- export to Git
- bidirectional Git sync

This is important to the product strategy: adoption should not require abandoning Git on day one.

## Hosted Architecture

Hosted Heddle has two major layers.

### Data plane

- repository object access
- refs and oplog behavior
- content inspection APIs
- sync and Git projection support

### Control plane

- namespaces
- repositories
- grants
- scoped authorization
- admin APIs
- eventual quotas, billing, and audit export

### Current hosted direction

- Postgres is the durable control-plane direction
- shared object storage is the hosted repository storage direction
- shared coordination should live outside process memory for horizontal scale
- some server-mode paths still need async-first cleanup for serious scale

## Web Product Positioning In The Architecture

The web app (now in the sibling **tapestry** repo) is not a browser IDE. It is an emerging hosted product for:

- repository inspection
- namespace and access operations
- change and attribution visibility
- future compare/review/history surfaces
- future workflows/builds/logs/artifacts surfaces

Some web routes are fully API-backed today. Others are foundation surfaces with partial or mock-backed UI. Product copy and docs should label that distinction rather than flattening it.

## Security And Verification

Current verification architecture includes:

- explicit principal and agent attribution in state metadata
- scoped hosted authorization
- signatures and verification metadata where supported
- structured denial reasoning in hosted paths where possible

Future verification work includes provenance-aware review workflows, richer verification surfaces, and build/workflow writeback into the repository record.

## Performance And Scale Themes

- immutable objects enable deduplication and safe reuse
- packfiles improve storage and transfer efficiency
- loose-plus-packed ref layout balances write speed and scale
- partial fetch negotiation, missing-blob tracking, and hosted hydration exist in foundation form; broader lazy retrieval productization remains in progress
- hosted scale work is increasingly about transport, coordination, metrics, and tenant isolation rather than only repository correctness

## Formal Specifications

Core state machines are specified in `specs/quint/` and mirrored by Rust property tests. When changing merge, refs, agents, worktrees, or related repository state-machine behavior, update the relevant spec as well as the implementation.

## Related Documentation

- `SPEC.md` in the sibling **weft** repo - formal behavior and storage/protocol truth
- `docs/HOSTED_NAMESPACES.md` in the sibling **weft** repo - hosted namespace and grant model
- `docs/HOSTED_ADMIN.md` in the sibling **weft** repo - hosted admin commands and API usage
- `docs/ENTERPRISE_BACKEND_ROADMAP.md` in the sibling **weft** repo - hosted platform roadmap
- `docs/RUNNERS_AND_BUILDS.md` in the sibling **weft** repo - hosted workflow and automation direction
- `docs/LINE_PROVENANCE_PLAN.md` in the sibling **weft** repo - provenance status and next-step roadmap
- `PRODUCT_SPEC.md` in the sibling **tapestry** repo - hosted web product scope and surface plan
