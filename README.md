# Heddle

[![Crates.io](https://img.shields.io/crates/v/heddle-cli.svg)](https://crates.io/crates/heddle-cli)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](https://www.rust-lang.org)

Heddle is an agent-native version control CLI written in Rust. It keeps its own state model and writes Git-compatible state through the checkout's real `.git`, adding:

- thread-first agent workflows (lightweight named work units with lifecycle, freshness, and promotion semantics)
- local captures and Git-compatible commits with explicit human and agent attribution
- content-addressed immutable history with stable change identifiers that survive rewrites
- provenance-aware inspection (`heddle query --attribution`, `heddle show`, `heddle diff`)

```bash
cargo install heddle-cli
cd /path/to/your/git/repo
heddle status              # inspect Git safely; Heddle prints the exact next command
heddle init                # add Heddle metadata; Git remains the source store
heddle verify
```

This repository ships the OSS CLI. The hosted backend (`weft`) and the web product (`tapestry`) are separate, closed-source repositories — see [Related projects](#related-projects).

## What `heddle` does today

In a plain Git repo, observe-only commands do not create `.heddle/`. `heddle status` first reports:

- the current Git branch
- dirty worktree/index state
- whether Heddle has been initialized
- the exact next command to initialize Git Overlay

Run the exact command printed by `heddle status`. In an existing Git checkout, that is `heddle init`: it creates the `.heddle` sidecar while the real `.git` remains authoritative for commits, trees, refs, packs, index, and worktree state. Heddle stores captures, threads, provenance, discussions, and source mappings in `.heddle`. Its embedded Sley engine powers the thin Git surface — `clone`, `commit`, `pull`, `push`, and `remote` — directly against `.git`. Heddle never requires the `git` executable. A retained `.heddle/git` Bridge Mirror is an internal projection cache used by explicit projection and maintenance paths, including undo recovery; it is never the authoritative Git store and remains scheduled for retirement.

`heddle adopt` atomically imports selected Git refs, makes Heddle the source authority, and enables the full Native Heddle feature set. The retained `.git` is then an explicit Git Projection adapter. Adoption is not required for normal Git Overlay use.

Heddle's CLI follows five operating principles — verification, disposability, composability, restraint, honesty — documented in [docs/PRINCIPLES.md](docs/PRINCIPLES.md).

## Capability status

### Shipped

- Content-addressed immutable state model
- Stable change identifiers
- Threads and markers
- Explicit principal and agent attribution
- Provenance-backed local blame with rewrite preservation across snapshot, collapse, and merge flows
- Semantic diff and compare
- Semantic integration by default: `ready`, `land`, and `sync` use AST-item-level merge within a file when built with the default `semantic` feature (first-class Rust/Python/JS/TS; Go/C/C++/Java opt-in); the engine does not auto-rewrite cross-file imports or call-sites
- Automatic state signing: device-local ed25519 identity minted on first use signs every authored state — provenance with no manual key setup
- Git overlay: direct `.git` integration, explicit native adoption, import, export, sync
- Byte-identical Git round-trip, CI-enforced: adopt→export reproduces identical commit/tree/blob/tag SHAs and a valid Git object graph, gated per-PR by 10 deterministic fixtures
- Multi-agent worktrees and agent registry

### Foundation in place

- Hosted client (`heddle-cli`'s optional `client` feature enables `dep:heddle-client` for talking to a hosted backend; `weft-client-shim` is always present as a non-optional dep)
- Verification and verification metadata across the wire protocol
- Commit-level visibility tiers: per-state `StateVisibility` records and `heddle visibility set/promote` verbs (with oplog tier records) are shipped client-side; Git projection visibility gating for exports and checkouts is landing; hosted serve-side enforcement is in progress

### Planned

- First-class history graph UX in the CLI
- Partial clone and lazy object retrieval

The 1.0 stability criterion — coverage thresholds, performance budgets, format/API stability, SemVer, and deprecation policy — lives in [docs/STABILITY.md](docs/STABILITY.md).

## Installation

### From crates.io

```bash
cargo install heddle-cli
```

The default feature set is `git-overlay`, `native`, `local`, `semantic`, `zstd`. To build a Git projection-only or native-only flavor, pass `--no-default-features --features git-overlay` or `--no-default-features --features native`.

### From source

Obtain a source checkout from the release archive or with any Git-compatible
client, then install it locally:

```bash
cd heddle
cargo install --path crates/cli
```

The client used to obtain the source is optional; Heddle itself does not invoke
or require it.

Prerequisites: Rust 1.85+, `cargo`, `rustfmt`, `clippy`.

## Getting Started

New to Heddle? In an existing Git checkout, start with `heddle status` and initialize the Git Overlay sidecar:

```bash
heddle status
heddle init
heddle capture -m "start project"
heddle commit
heddle push
```

For a new or unborn repo, initialize once, set attribution if needed, then use `heddle capture` as the Heddle save boundary:

```bash
heddle init --principal-name "Ada Lovelace" --principal-email ada@example.com
heddle capture -m "start project"
```

In a Git checkout, `heddle init` creates the Heddle sidecar and leaves source storage in the checkout's real `.git`. `heddle capture` records Heddle metadata and provenance in `.heddle`; `heddle commit`, `pull`, `push`, and `remote` delegate through Sley to `.git`. `heddle commit` commits the complete captured tree, replaces the Git index with that tree, and does not run Git `pre-commit` or `commit-msg` hooks. Use `heddle adopt` when you want an atomic transition to Heddle-native source authority and its full feature set.

### The verb-by-verb tour

```bash
# In an ordinary Git repo: inspect, initialize Git Overlay, and verify
heddle status
heddle init
heddle verify

# Save Heddle provenance, then commit Git-owned source history
heddle capture -m "add user authentication"
heddle commit

# Start isolated work and prove it is ready
heddle start feature/auth --path ../feature-auth
cd ../feature-auth
heddle capture -m "add auth validation"
heddle ready

# Land locally, then publish with the source authority
heddle land --thread feature/auth
heddle push

# Inspect history and provenance
heddle log
heddle diff HEAD~1 HEAD
heddle query --attribution path/to/file.rs
```

`heddle status` reports the current branch or thread, what is dirty, whether another operation is in progress, and the recommended next command. The same `recommended_action` field is carried through `heddle doctor`, `heddle thread show`, and `heddle status` for programmatic use.

In Git-overlay repositories, `heddle land` projects a landed thread as one atomic Git commit by default. The original per-State captures remain in Heddle history. Use `heddle land --no-squash` for a single invocation that should export one Git commit per State.

## Core concepts

| Concept | Description |
|---------|-------------|
| **State** | Immutable snapshot of a repository at a point in time |
| **ChangeId** | Per-state identifier. Each state carries a *logical* ChangeId (the same value is carried forward across rewrites) and a *physical* ChangeId minted fresh for that state. The `hd-…` shown in output is the **physical** id; the logical id is internal and not surfaced (see [Identifiers in output](#identifiers-in-output)) |
| **ContentHash** | BLAKE3 hash of object contents for integrity and deduplication |
| **Thread** | Mutable named reference to a state |
| **Marker** | Immutable named reference to a state |
| **Principal** | Human identity accountable for a change |
| **Agent** | Model identity associated with a change |

### Identifiers in output

History commands render up to three distinct identifiers. They are not interchangeable:

- **`hd-…` change id** (e.g. `hd-wgqnj47xyh40`) — the **physical ChangeId**, minted fresh for each state. It is the handle for *this specific state*: pass it to commands that take a change as an argument — `heddle show <id>`, `heddle query --attribution` reports it per line, and `heddle log <id>` selects by it (resolution matches the physical id of a recorded state). Prefixes are accepted, so a short `hd-…` is enough as long as it is unambiguous. It is **not** a lineage handle that survives rewrites: amending or rebasing produces a *new* state with a *new* `hd-…`, so an `hd-…` captured before a rebase still resolves to the pre-rebase state, not the rewritten one. Heddle does track a separate stable *logical* ChangeId that is carried forward across a rewrite, but it is internal — it is not rendered in output and is not accepted as a command argument, so the displayed `hd-…` is the only id you can pass, and it identifies one state rather than a lineage.
- **`(……)` content hash** (e.g. `(61408ef9)`, shown beside the change id by `heddle log --verbose` and `heddle show`) — the short form of the **ContentHash**, a BLAKE3 digest of the state's contents. It is *not* a Git commit sha. Because it is content-addressed, it changes whenever the state's content changes, so it pins an exact snapshot but is not a stable handle to "the change". Use it for integrity/equality checks, not as a command argument.
- **Git checkpoint sha** (shown on the `Git checkpoint:` line under `heddle log --verbose` / `heddle show`) — the actual Git commit that binds the state into Git history. This is the handle for any Git-compatible client; Heddle commands take the `hd-…` change id instead.

Rule of thumb: hand `hd-…` change ids to Heddle, and the checkpoint sha to a Git-compatible client.

## Agent-friendly output

Heddle is designed for programmatic use by agents and automation. Text is always the default, including when stdout is piped. Pass `--output json` for the full machine contract or `--output json-compact` for the smaller decision surface:

```bash
heddle status --output json
heddle doctor --output json
heddle diff --output json
heddle log --output json
heddle show HEAD --output json
```

See [.agents/agent-workflows.md](.agents/agent-workflows.md) for durable automation guidance.

For idempotent retries, inspect `heddle help --output json` before
using `--op-id` or `HEDDLE_OPERATION_ID`. Commands with
`supports_op_id: true` support caller-supplied explicit replay: the same
id plus the same arguments replays the recorded outcome, while the same
id with different arguments fails with a typed conflict. `persists_op_id`
is reserved for commands that generate and save an id across interrupted
retry loops; current replay-safe automation should supply an explicit
UUID. `--op-id` is intentionally not advertised as a broad global option
in the command catalog; use each command's `op_id_behavior` field.

## Configuration

Heddle uses three local config scopes:

- `UserConfig` (`~/.config/heddle/config.toml`) — user identity, agent defaults, output preferences, hosted-client credentials
- `RepoConfig` (`.heddle/config.toml`) — repository source authority, local behavior, ignore defaults, storage coordinates, remote aliases, repository format version
- `WorktreeState` — per-checkout runtime state (current session, segment) tracked separately from repo config

Set `[land] squash = false` in user config to make `heddle land` preserve per-State Git export by default. The command-line `--no-squash` flag provides the same opt-out for one land operation.

Precedence: CLI flags override the relevant config scope; env vars override file-backed config where supported; repo config stays repo-local; worktree state stays checkout-local.

Useful environment variables:

```bash
export HEDDLE_AGENT_PROVIDER="anthropic"
export HEDDLE_AGENT_MODEL="claude-opus-4-7"
export HEDDLE_PRINCIPAL_NAME="Your Name"
export HEDDLE_PRINCIPAL_EMAIL="you@example.com"

export RUST_LOG=heddle=debug
export RUST_BACKTRACE=1
```

Heddle records the agent model string verbatim and echoes it back in attribution output (for example the `Agent:` line of `heddle log --verbose`). If the coding agent reports a model id with a bracketed suffix — such as `claude-opus-4-8[1m]` — heddle preserves the suffix as-is; it does not add or interpret it. The suffix is supplied by the agent harness to distinguish a model variant (for Claude models the bracketed tag denotes the context-window variant), so set `HEDDLE_AGENT_MODEL` to whatever identifier you want recorded.

## Repository layout

This repository is a Cargo workspace. The OSS crates live under `crates/`:

```text
crates/cli/                 # the `heddle` binary
crates/cli-shared/          # config types shared between cli and other surfaces
crates/objects/             # core object and repository model
crates/repo/                # repository helpers and higher-level repo operations
crates/refs/                # threads, markers, HEAD, packed refs
crates/oplog/               # undo/redo oplog model
crates/semantic/            # semantic diff and code-aware analysis
crates/merge/               # merge core
crates/review/              # review primitives
crates/state_review/        # state-level review helpers
crates/ingest/              # `heddle-ingest` binary and Git import path
crates/wire/                # native Heddle wire protocol types
crates/grpc/                # gRPC client and server transport
crates/client/              # local-side hosted client
crates/weft-client-shim/    # shim used by the `client` feature to talk to weft
crates/crypto/              # crypto primitives
crates/daemon/              # background daemon
crates/devtools/            # developer tooling
crates/mount/               # filesystem mount support
crates/runtime-bridge/      # runtime bridge between cli and async server stacks

docs/                       # architecture, principles, stability, design notes
specs/                      # Quint formal specifications
tests/                      # integration and property tests
```

Architecture and runtime details are in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Development

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

Contributor rules and documentation truth guidance are in [AGENTS.md](AGENTS.md).

## Related projects

Heddle is one of three repositories under [HeddleCo](https://github.com/HeddleCo):

- **[HeddleCo/heddle](https://github.com/HeddleCo/heddle)** (this repo) — the OSS CLI, Apache-2.0
- **[HeddleCo/weft](https://github.com/HeddleCo/weft)** — closed-source hosted backend that provides hosted namespaces, repositories, grants, and the server side of the wire protocol that `heddle`'s `client` feature talks to
- **[HeddleCo/tapestry](https://github.com/HeddleCo/tapestry)** — closed-source SvelteKit marketing site and hosted web app

The hosted control plane, web UI, and any "hosted X" surfaces live in `weft` and `tapestry`. This repository contains only the OSS CLI and its supporting crates.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).

---

Heddle is still alpha software. Storage formats, APIs, and the wire protocol are evolving quickly.
