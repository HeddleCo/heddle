# Heddle

[![Crates.io](https://img.shields.io/crates/v/heddle-cli.svg)](https://crates.io/crates/heddle-cli)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](https://www.rust-lang.org)

Heddle is an AI-native version control CLI written in Rust. It runs as a Git overlay on top of an existing Git repository, adding:

- thread-first agent workflows (lightweight named work units with lifecycle, freshness, and promotion semantics)
- local captures and checkpoints with explicit human and agent attribution
- content-addressed immutable history with stable change identifiers that survive rewrites
- provenance-aware inspection (`heddle blame`, `heddle inspect`, `heddle compare --semantic`)

```bash
cargo install heddle-cli
cd /path/to/your/git/repo
heddle status        # bootstraps a .heddle/ sidecar and adopts the current branch
```

This repository ships the OSS CLI. The hosted backend (`weft`) and the web product (`tapestry`) are separate, closed-source repositories — see [Related projects](#related-projects).

## What `heddle` does today

In a plain Git repo, `heddle` auto-adopts:

- the current checkout and dirty state
- the active local branch as the current Heddle thread
- other local branch tips as lightweight thread mirrors

That means `heddle status`, `heddle thread list`, and `heddle workspace show` work immediately, with no `init` step. `heddle bridge git import` is the explicit step when you want richer ancestry or full Heddle history semantics for a specific branch or tag (`heddle bridge git import --ref <branch-or-tag>`).

Heddle's CLI follows five operating principles — trust, disposability, composability, restraint, honesty — documented in [docs/PRINCIPLES.md](docs/PRINCIPLES.md).

## Capability status

### Shipped

- Content-addressed immutable state model
- Stable change identifiers
- Threads and markers
- Explicit principal and agent attribution
- Provenance-backed local blame with rewrite preservation across snapshot, collapse, and merge flows
- Semantic diff and compare
- Git overlay: adopt, import, export, sync
- Multi-agent worktrees and agent registry

### Foundation in place

- Hosted client (`heddle-cli`'s optional `client` feature enables `dep:heddle-client` for talking to a hosted backend; `weft-client-shim` is always present as a non-optional dep)
- Verification and trust metadata across the wire protocol

### Planned

- First-class history graph UX in the CLI
- Partial clone and lazy object retrieval

The 1.0 stability criterion — coverage thresholds, performance budgets, format/API stability, SemVer, and deprecation policy — lives in [docs/STABILITY.md](docs/STABILITY.md).

## Installation

### From crates.io

```bash
cargo install heddle-cli
```

The default feature set is `git-overlay`, `native`, `local`, `semantic`, `zstd`. To build a Git-overlay-only or native-only flavor, pass `--no-default-features --features git-overlay` or `--no-default-features --features native`.

### From source

```bash
git clone https://github.com/HeddleCo/heddle
cd heddle
cargo install --path crates/cli
```

Prerequisites: Rust 1.85+, `cargo`, `rustfmt`, `clippy`.

## Quickstart

```bash
# In an ordinary Git repo: bootstrap and inspect
heddle status

# Start a thread, capture work, checkpoint to a Git-facing commit
heddle start feature/auth
heddle capture -m "add auth validation"
heddle checkpoint -m "add user authentication"

# Preview a merge, then merge and push
heddle merge feature/auth --preview
heddle merge feature/auth
heddle push

# Inspect history and provenance
heddle log
heddle compare HEAD~1 HEAD --semantic
heddle blame path/to/file.rs
```

`heddle status` is the control tower: it reports the current branch or thread, what is dirty, whether another operation is in progress, and the recommended next command. The same `recommended_action` field is carried through `heddle diagnose`, `heddle thread show`, and `heddle workspace show` for programmatic use.

## Core concepts

| Concept | Description |
|---------|-------------|
| **State** | Immutable snapshot of a repository at a point in time |
| **ChangeId** | Stable logical identifier that survives rewrites |
| **ContentHash** | BLAKE3 hash of object contents for integrity and deduplication |
| **Thread** | Mutable named reference to a state |
| **Marker** | Immutable named reference to a state |
| **Principal** | Human identity accountable for a change |
| **Agent** | Model identity associated with a change |

## Agent-friendly output

Heddle is designed for programmatic use by agents and automation. Most read-shaped commands take `--output json` (the legacy `--json` flag is deprecated; `--output auto` — the default — renders text on a TTY and JSON when stdout is piped):

```bash
heddle status --output json
heddle diagnose --output json
heddle diff --output json
heddle log --output json
heddle show HEAD --output json
```

See [.agents/agent-workflows.md](.agents/agent-workflows.md) for durable automation guidance.

## Configuration

Heddle uses three local config scopes:

- `UserConfig` (`~/.config/heddle/config.toml`) — user identity, agent defaults, output preferences, hosted-client credentials
- `RepoConfig` (`.heddle/config.toml`) — repository-local behavior, ignore defaults, storage coordinates, remote aliases, repository format version
- `WorktreeState` — per-checkout runtime state (current session, segment) tracked separately from repo config

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
crates/proto/               # wire protocol types
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
