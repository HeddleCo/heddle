# Heddle

[![Crates.io](https://img.shields.io/crates/v/heddle-cli.svg)](https://crates.io/crates/heddle-cli)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](https://www.rust-lang.org)

```
cargo install heddle-cli
heddle bridge git-import .
```

Heddle is an AI-native version control system written in Rust. It combines content-addressed storage, immutable history with stable change identifiers, explicit human and agent attribution, and a Git-overlay mode that drops into existing repositories without changing storage or remote model on day one.

The main public on-ramp is Git-overlay adoption: drop `heddle` into an existing Git repository and get thread-first agent workflows, local captures, provenance-aware inspection, and explicit Git checkpoints without changing your team's storage or remote model on day one.

In a normal Git repo, `heddle` now auto-adopts:

- the current checkout and dirty state
- the active local branch as the current Heddle lane
- other local branch tips as lightweight thread mirrors

That means `heddle status`, `heddle thread list`, and `heddle workspace show` work immediately in an existing Git repo. Full `heddle bridge git import` is still the explicit step when you want deeper history import, richer ancestry, and fuller compare/log behavior across those branches. If you only need one Git-only branch to become a full Heddle history lane, use `heddle bridge git import --ref <branch>`.
Git tags stay explicit: `heddle show <tag>` and other history-oriented commands will tell you to import just that tag with `heddle bridge git import --ref <tag>` when needed.

The repository is being organized around two binaries:

- `heddle` for local repository work plus hosted client operations
- `hosted` for hosted server and admin operations

## Why Heddle?

Git was built for human-only workflows. Modern teams now need stronger answers to problems Git and Git hosting still treat as incidental:

- **Stable change identity** - logical changes keep durable IDs even when history is rebased, collapsed, or force-pushed
- **Explicit human and AI attribution** - every state can carry principal identity, agent provider/model, confidence, and verification metadata
- **Attached reasoning** - rule, why, gotcha, and migration guidance can travel with the code they govern instead of disappearing into PR comments and chat logs
- **Hosted control plane** - namespaces, repositories, grants, and access inheritance are first-class, not bolted on around a flat repo list
- **Repository intelligence** - hosted surfaces can inspect history, trust, and operational state from one place
- **Git interoperability** - Heddle can import, export, and sync with Git so adoption can be incremental

Heddle's CLI follows five operating principles — trust, disposability, composability, restraint, honesty. They're documented in [docs/PRINCIPLES.md](docs/PRINCIPLES.md), and `heddle doctor docs` keeps the docs honest about the actual CLI surface.

## Capability Status

### Shipped

- Content-addressed immutable state model
- Stable change identifiers
- Threads and markers
- Explicit principal and agent attribution
- Provenance-backed local blame with rewrite preservation across snapshot, collapse, and merge flows
- Semantic diff support
- Git bridge and remote sync
- Multi-agent worktrees and agent registry

### Foundation in place

- Hosted namespaces, repositories, and grants
- Hosted web product for repository inspection and operations
- Repository and namespace views backed by hosted content/admin APIs
- Revisable context annotations as a core Heddle concept, with CLI, hosted APIs, and state-aware web inspection in place
- Hosted provenance and context read APIs for file and change inspection
- Hosted context write APIs for create, revise, supersede, and suggestion-backed inspection
- Verification and trust metadata across CLI, server, and web layers

### Planned

- Full compare and review surfaces in the web app
- First-class history graph UX
- Provenance-aware compare and review workflows beyond current file/change inspection
- Hosted builds, workflows, logs, artifacts, and verification writeback
- Global search and command palette
- Partial clone and lazy object retrieval

## Quick Start

```bash
# In an ordinary Git repo, Heddle can bootstrap its sidecar on first use
heddle status

# Initialize a new repository
heddle init

# Start a thread
heddle start feature/auth

# Capture a recoverable sub-commit step with intent
heddle capture -m "Add auth validation"

# Bundle the current capture chain into the Git-facing commit boundary
heddle checkpoint -m "Add user authentication"

# Inspect the current thread
heddle status

# Preview integration
heddle merge feature/auth --preview

# Merge and push the current thread
heddle merge feature/auth
heddle push

# View history
heddle log

# Inspect a thread or state
heddle inspect feature/auth

# Compare two states
heddle compare HEAD~1 HEAD --semantic
```

## Core Concepts

| Concept | Description |
|---------|-------------|
| **State** | Immutable snapshot of a repository at a point in time |
| **ChangeId** | Stable logical identifier that survives rewrites |
| **ContentHash** | BLAKE3 hash of object contents for integrity and deduplication |
| **Thread** | Mutable named reference to a state |
| **Marker** | Immutable named reference to a state |
| **Principal** | Human identity accountable for a change |
| **Agent** | Model identity associated with a change |
| **Namespace** | Hosted organizational container for repositories and grants |
| **Grant** | Hosted access rule applied to a namespace or repository |

## Installation

### From source

```bash
git clone https://github.com/HeddleCo/heddle
cd heddle
cargo build --release
cargo install --path .
```

### Prerequisites

- Rust 1.85+
- `cargo`, `rustfmt`, `clippy`

## Common Commands

### Core thread workflow

```bash
heddle init [PATH]
heddle status [--short] [--watch]
heddle diagnose [--profile]
heddle workspace show [--watch]
heddle start <name> [--workspace auto|heavy|light] [--path <dir>]
heddle capture -m "message"
heddle checkpoint [-m "message"]
heddle thread captures <thread> [--limit N]
heddle log [--oneline] [--graph]
heddle show <state>
heddle inspect [state|thread]
heddle merge <thread> --preview
heddle merge <thread> [-m "message"]
heddle collapse <states...> --into <name>
heddle push [<remote>] [--force]
```

### Thread maintenance

```bash
heddle thread list
heddle thread show <name> [--watch]
heddle thread refresh <name>
heddle thread move <from> <to> --path <path>
heddle thread absorb <name> [--into <parent>]
heddle thread resolve <name>
heddle thread promote <name> [--path <dir>]
heddle thread drop <name>
heddle fork [--name <name>]
```

### Advanced workflow and automation

```bash
heddle run --thread <name> -- <cmd...>
heddle ready [--thread <name>] [-m "message"]
heddle continue
heddle abort
heddle sync [--thread <name>]
heddle ship [--thread <name>] [--push]
heddle delegate [--parent <name>] [--workspace auto|heavy|light] <tasks...>
```

The simplified operator loop is documented in [docs/OPERATOR_LOOP.md](/Users/lukethorne/.codex/worktrees/aaec/heddle/docs/OPERATOR_LOOP.md).

Recommended first-run loop in an existing Git repo:

```bash
heddle status
heddle continue
heddle abort
heddle sync
```

`heddle status` should tell you which branch or thread you are on, what is dirty, whether another operation is in progress, and the primary recommended next step. The same recommended next step is then carried through `diagnose`, `thread show`, and `workspace show` via the JSON/text `recommended_action` surface.

### Advanced debugging and provenance

```bash
heddle actor list
heddle actor show [<session>]
heddle actor explain [<session>]
heddle session list
heddle session show [<session>]
heddle presence publish --session <id>   # built with the `hosted-client` feature
```

### History, inspection, and maintenance

```bash
heddle capture -m "message"
heddle checkpoint [-m "message"]
heddle goto <state> [--force]
heddle diff [from] [to]
heddle compare <a> <b> [--semantic]
heddle clean [--force]
heddle fsck [--full] [--repair]
heddle maintenance inspect
heddle maintenance run
heddle maintenance gc [--prune]
heddle undo [-n <steps>] [--list]
heddle redo [-n <steps>]
heddle blame <file> [--state <id>]
heddle rebase <thread>
heddle cherry-pick <state>
heddle revert <state>
```

### Markers

```bash
heddle marker list
heddle marker create <name> [state]
heddle marker delete <name>
heddle marker show <name>
```

### Remote and Git bridge

```bash
heddle remote add <name> <url>
heddle fetch [<remote>] [--all]
heddle push [<remote>] [--thread <name>] [--force]
heddle pull [<remote>] [--thread <name>]
heddle clone <remote> <local>

heddle bridge git import [--path <path-or-url>] [--ref <branch-or-tag>]
heddle bridge git export <path>
heddle bridge git sync
```

### Thread workflow

```bash
heddle start <thread> [--workspace auto|heavy|light] [--path <dir>] [--task <goal>]
heddle status
heddle workspace show
heddle merge <thread> --preview
heddle merge <thread>
heddle thread show <thread>
heddle push
```

Important current behavior:

- `heddle start` is the public entrypoint for starting work; `--workspace heavy` creates a real checkout, `--workspace light` uses the virtualized filesystem path, and `--path` places a heavy checkout somewhere explicit
- in a plain Git repository, `heddle status` bootstraps Heddle sidecar storage under `.heddle/`, adopts the active Git branch as the current Heddle thread, and reports dirty files immediately
- `heddle init` in a Git-backed repository uses the same sidecar model explicitly if you want to opt in ahead of first status/diagnose usage
- `heddle capture` records a fine-grained recoverable step for undo, provenance, and review; this is the thing agents should do after a turn, before the work is ready to become public history
- `heddle checkpoint` bundles the current capture chain into the Git-facing commit boundary and records the mapping back to the Heddle change ID
- `heddle thread captures <thread>` shows the granular capture trail behind a thread, while `heddle thread show <thread>` includes the latest captures inline
- in git-overlay repositories, `heddle ship` auto-checkpoints the integrated result into Git after a clean merge path
- in git-overlay repositories, `heddle clone`, `heddle fetch`, `heddle pull`, and `heddle push` use Heddle's native Git plumbing for local/file and network Git remotes; no `git` binary is required for normal overlay flows
- `heddle fsck --bridge` validates the Git-overlay mirror, mapping sidecar, Heddle notes, thread refs, and attached checkout branch
- `heddle agent reserve|heartbeat|release|list` provides a JSON-first reservation API so external agents can coordinate one writer per thread without scraping human output
- `heddle bridge git import` defaults `--path` to the current repo, and you can still point it at another local repo or URL with `--path <path-or-url>`
- Git tags are not auto-adopted as threads; import them explicitly with `heddle bridge git import --ref <tag>` when you want full Heddle history semantics for a tag target
- `auto` defaults to a Heddle-managed heavy checkout; the managed-vs-custom distinction is just path placement, not a separate workspace mode
- `heddle workspace show` is the repo-wide control tower; it groups current, stacked, parallel, ready, blocked, and recently merged threads
- `heddle diagnose` gives a one-command local handoff packet: repository, current thread, actor/session context, dirty paths, workspace counts, health, next command, and optional read-path timing with `--profile`
- `heddle ready` captures outstanding work if needed, evaluates semantic merge readiness, and moves the thread to `ready` or `blocked`
- `heddle status` acts as the control tower, including thread health and the next recommended command
- `heddle thread show/list/refresh/move/absorb/resolve/promote/drop` provide the expert maintenance surface for thread lifecycle and work shaping
- `heddle thread show --watch` and `heddle status --watch` keep the same thread-first mental model for live updates
- `heddle capture --split --into <thread> --path <path>` moves selected dirty paths into another thread without forcing Git-style branch surgery
- lightweight threads track base root, lifecycle, changed paths, impact categories, task metadata, freshness, and promotion warnings in persisted thread records
- `heddle merge <thread> --preview` now produces a structured decision report with blockers and a recommended next step
- plain `heddle push` pushes the current attached thread by default
- `heddle undo` and `heddle redo` operate on the current thread only, using the active checkout scope

## Agent-Friendly Output

Heddle is designed for programmatic use by agents and automation.

```bash
heddle status --json
heddle diagnose --json
heddle diagnose --profile --json
heddle diff --json
heddle log --json
heddle show HEAD --json

heddle status --output json
heddle status --output text
```

See `.agents/agent-workflows.md` for workflow guidance and caveats.

## Configuration Model

Heddle uses four scopes:

- `UserConfig` for user identity, agent defaults, output preferences, auth profiles, and client-side logging/tracing defaults
- `RepoConfig` for repository-local behavior, ignore defaults, storage coordinates, and remote aliases in `.heddle/config.toml`
- `ServerConfig` for hosted server storage, database, auth, TLS, and admission settings
- `WorktreeState` for per-checkout runtime state such as the current session and segment

Precedence is intentionally explicit:

- CLI flags override the relevant config scope
- env vars override the relevant file-backed config when supported
- repo config stays repo-local
- server config stays server-local
- worktree state stays checkout-local and separate from repo config

Default locations:

- user config: `~/.config/heddle/config.toml`
- repo config: `.heddle/config.toml`
- worktree state: a separate per-checkout state file
- server config: a dedicated server config file or server-specific env overrides

Repo config is where repository format versioning lives. User identity, agent defaults, and hosted client credentials should not be stored there.

### Environment variables

```bash
# Agent attribution
export HEDDLE_AGENT_PROVIDER="anthropic"
export HEDDLE_AGENT_MODEL="claude-opus-4-7"

# Principal attribution
export HEDDLE_PRINCIPAL_NAME="Your Name"
export HEDDLE_PRINCIPAL_EMAIL="you@example.com"

# Development
export RUST_LOG=heddle=debug
export RUST_BACKTRACE=1
```

`HEDDLE_SESSION_ID` and `HEDDLE_SESSION_SEGMENT` are not part of repo config. Worktree runtime state is tracked separately from the repository configuration file.

## Development

### Build and test

```bash
cargo build
cargo build --release
cargo test
cargo test -- --nocapture
cargo clippy -- -D warnings
cargo fmt --check
```

### Workspace structure

```text
crates/
  cli/         # local CLI entry point and hosted client dispatch
  objects/        # core object and repository model
  repo/        # repository helpers and higher-level repo operations
  refs/        # threads, markers, HEAD, packed refs
  oplog/       # undo/redo oplog model
  server/      # hosted server library and admin/content APIs
  hosted/      # hosted server/admin binary
  heddle-bridge/      # Git interoperability
  semantic/    # semantic diff and code-aware analysis
  ...
docs/               # architecture, hosted model, roadmap, future-state plans
web/                # SvelteKit marketing site and hosted app
specs/              # Quint formal specifications
tests/              # integration and property tests
```

## Documentation

- `SPEC.md` - formal behavior and storage/protocol truth
- `docs/ARCHITECTURE.md` - system architecture
- `docs/HOSTED_NAMESPACES.md` - hosted namespace and grant model
- `docs/HOSTED_ADMIN.md` - hosted admin surfaces and command/API usage
- `docs/ENTERPRISE_BACKEND_ROADMAP.md` - platform roadmap
- `docs/RUNNERS_AND_BUILDS.md` - hosted workflows/builds plan
- `docs/LINE_PROVENANCE_PLAN.md` - provenance status, shipped behavior, and next steps
- `AGENTS.md` - contributor rules and documentation truth guidance
- `.agents/agent-workflows.md` - durable automation guidance
- `web/PRODUCT_SPEC.md` - web product scope and future-state surfaces

## Current Limitations

- Semantic diff is available but may be conservative
- Partial fetch and missing-blob hydration have landed in foundation form, but general partial clone / lazy fetch productization remains in progress
- Hosted provenance-backed review remains foundation-level; richer compare/review workflows are still planned
- Hosted builds, workflows, artifacts, and verification writeback are planned, not implemented
- `heddle undo` and `heddle redo` are thread-scoped to the current checkout; they do not rewind work recorded from another checkout's HEAD path

## Known Trade-offs

These are deliberate edges. They're listed here so they don't surprise you when you hit them.

- **The verify hook is fail-closed.** When Heddle's PostToolUse hook on `Bash` sees a definitive failure signal (`test result: FAILED`, `error[E…]`, `^error:`) in test output, it writes a `failed-*` marker instead of capturing. Ambiguous output is a no-op — no capture, no marker. The intent is to avoid green-by-default attribution when the signal is unclear; the cost is that partial or interrupted runs leave no audit trail. If you want a permissive verify behavior, override the hook.
- **`heddle start` does not change your shell's cwd.** The new thread's checkout lives at `<repo>/.<thread>-heddle-threads/<name>/root`, but Heddle is a child process and can't reach into the parent shell. `heddle start <name>` prints a copy-pasteable `cd` line; shell wrappers can use `dir=$(heddle start <name> --print-cd-path) && cd "$dir"` for one-step automation.
- **The Stop hook only fires at turn end.** Auto-capture runs when the assistant finishes a turn, not within a single turn. For in-turn smoke tests, rely on the verify-hook capture (after a Bash test invocation) rather than expecting Stop to fire mid-loop.
- **`heddle undo` is thread-local.** Already in the limitations list above, repeated here because it's the one that bites most often when context-switching across checkouts: switch to the originating thread before undoing work recorded there.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contributing

Contributions are welcome. Start with `AGENTS.md` and the relevant docs before changing behavior or product copy.

---

Heddle is still alpha software. Storage formats, APIs, and hosted product surfaces are evolving quickly.
