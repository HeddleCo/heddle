# Useful Commands

## Build

```bash
cargo build              # Debug build
cargo build --release    # Release build
```

## Test

```bash
cargo test                    # All tests
cargo test -- --nocapture     # Show println output
cargo test <test_name>        # Run specific test
```

## Lint

```bash
cargo clippy -- -D warnings   # Lint with warnings as errors
cargo fmt                     # Format code
cargo fmt --check             # Check formatting
```

## Documentation

```bash
cargo doc --open              # Generate and open docs
```

## Run

```bash
# Local/client binary
cargo run -p cli -- init
cargo run -p cli -- status
cargo run -p cli -- capture -m "message"
cargo run -p cli -- actor list
cargo run -p cli -- actor explain
cargo run -p cli -- integration install claude-code

# Hosted server/admin binary
cargo run -p hosted -- serve --bind 0.0.0.0 --port 8421
```

## Threads, Actors, And Worktrees

```bash
# Create an isolated agent checkout sharing the object store
cargo run -- start feature/auth --path /tmp/agent-a

# Spawn an explicit Heddle actor (creates thread + registry entry)
cargo run -- actor spawn --thread feature/x

# List active actors
cargo run -- actor list

# Inspect why Heddle attached an actor (omit the session id to use the current thread's actor)
cargo run -- actor explain agent-x7k9qm4h

# Mark actor complete
cargo run -- actor done --session agent-x7k9qm4h
```

Notes:

- Threads are the human-facing work context.
- Actors are the active workers on those threads.
- Heddle sessions and segments are the execution records underneath that actor model.
- Supported harnesses may create actors ambiently; explicit `actor spawn` is not required for the ambient path.

## Harness Integration

```bash
# Offer optional harness install during init
cargo run -- init --install-harnesses auto

# Install harness integrations on an existing repo
cargo run -- integration install claude-code --scope repo
cargo run -- integration install opencode --scope repo
cargo run -- integration install codex --scope user

# Check Heddle-managed harness integration health
cargo run -- integration doctor
cargo run -- integration list
```

Internal plumbing:

```bash
# Internal bridge protocol entrypoint
cargo run -- harness-bridge

# Internal relay used by installed hooks/plugins
cargo run -- integration relay claude-code SessionStart
```

## History Operations

```bash
# Rebase current thread onto another (replays commits as new states)
cargo run -- rebase <thread>
cargo run -- rebase --continue     # After resolving conflicts
cargo run -- rebase --abort        # Cancel in-progress rebase

# Collapse (squash) multiple states into one
cargo run -- collapse <from>..<to>

# Cherry-pick a specific state onto HEAD
cargo run -- cherry-pick <state-id>

# Fork current state (same tree, new change_id)
cargo run -- fork

# Revert a state (creates inverse change)
cargo run -- revert <state-id>

# Undo/redo last operation(s)
cargo run -- undo
cargo run -- redo
```

Note: rebase and collapse create **new** state objects — originals remain in the store.
Force push (`heddle push --force`) is required after rebase since the thread is non-fast-forward.

## Debug Build

```bash
RUSTFLAGS="-C debuginfo=2" cargo build
```

## Hosted Local Services

```bash
railway status
railway dev up
railway run -s Postgres -- env
```

## Container Build

```bash
docker build -t heddle-enterprise-backend:test .
docker run --rm heddle-enterprise-backend:test --help
```

## Web App (SvelteKit)

```bash
cd web
bun install          # Install dependencies
bun run dev          # Dev server (default port 5173)
bun run build        # Production build
bun run preview      # Preview production build
npx svelte-check     # Type-check all Svelte + TS files
```

Environment variables for the web app (`.env` in `web/`):
```
HEDDLE_API_URL=http://localhost:8080    # Rust server base URL
HEDDLE_API_TOKEN=<admin-token>          # Static admin token
HEDDLE_SERVER_BISCUIT_PRIVATE_KEY=<pem> # Server-side only; used by hosted for Biscuit signing
```
