# Common Tasks

## Adding a New CLI Command

1. Add the command variant in `crates/cli/src/cli/cli_args/`
2. Implement the command under `crates/cli/src/cli/commands/`
3. Wire dispatch in `crates/cli/src/main.rs` or the relevant command module
4. Add tests in the appropriate crate or integration test file under `tests/`
5. Update documentation (README.md, CHANGELOG.md)

## Adding a New Object Type

1. Define the type in the owning crate, usually under `crates/core/src/` or `crates/repo/src/`
2. Implement `Serialize`/`Deserialize` with serde
3. Add content hash computation method
4. Add or update storage helpers in the relevant store/backend crate
5. Re-export from the owning crate if needed

## Modifying the Specification

1. Update `SPEC.md` first
2. Implement the changes to match the spec
3. Add/update tests
4. Document breaking changes
5. If the change touches a state machine (merge, refs, locks, agents, worktrees, repo ops), update the corresponding Quint spec in `specs/quint/` and the Rust property tests in `tests/formal_specs.rs` — see [[.agents/formal-specs]]

## Modifying State Machine Logic

When adding or changing guards, transitions, or invariants in any core state machine:

1. Update the Quint spec first — add/modify actions, adjust guards, update invariants
2. Run `quint run --max-samples=10000 --max-steps=20 --invariant=safety specs/quint/<spec>.qnt`
3. Implement the Rust change
4. Update the corresponding `mod` in `tests/formal_specs.rs`
5. If the change was prompted by a bug, add a regression trace (`run REG-N = ...`) to the Quint spec
6. Run `./specs/quint/verify.sh` to confirm everything passes

## Git Workflow

This repository uses Heddle for version control (dogfooding), but may also maintain a Git mirror for GitHub integration.

When making changes:
1. Use `heddle capture` with descriptive `-m "message"` (the `snapshot` alias is hidden — prefer `capture`)
2. Include `--confidence` when appropriate
3. Run `cargo test` and `cargo clippy -- -D warnings` before committing

## Multi-Agent Work

1. Use `heddle start <name> --path <dir>` when you need a real isolated checkout
2. Use `heddle actor spawn --thread <name> --provider ... --model ...` when you need explicit actor metadata on a thread
3. Run `heddle undo` from the specific isolated checkout you want to rewind; undo is thread-local

## Hosted Backend Changes

1. Read `SPEC.md`, `docs/HOSTED_ADMIN.md`, and `docs/HOSTED_NAMESPACES.md` in the sibling **weft** repo first
2. Keep durable metadata in Postgres and object content in shared object storage
3. Prefer external/shared admission-control state for horizontally scaled behavior
4. Add targeted tests for hosted authz, admin surfaces, and feature-gated Postgres paths

## Adding a Content API Endpoint

Content routes live in the hosted server implementation and are intercepted before the admin auth gate:

1. Add the new content query/helper in the relevant hosted server module
2. Wire it into both filesystem-backed and Postgres-backed paths if both modes are supported
3. Add or update route interception for the content prefix if needed
4. Add the corresponding typed method to the server-side API client in the sibling **tapestry** repo
5. Create or update the relevant SvelteKit server loader in tapestry to call the new method

Keep the implementation model consistent with the owning server path. Before changing sync/async boundaries, read the hosted server architecture docs and the current module shape. Note the hosted server now lives in the sibling **weft** repo and the SvelteKit web product in **tapestry**.

## Wiring a New Web Page

> The SvelteKit web product lives in the sibling **tapestry** repo. The steps below describe its loader conventions; make the edits there, not in this workspace.

1. Create a `+page.server.ts` (not `+page.ts`) in the route directory — server-only loaders keep API creds server-side
2. Import the server-side `api` client and call the appropriate content/admin methods
3. Handle errors with `throw error(status, message)` from `@sveltejs/kit`
4. Wrap parallel fetches in `Promise.all([...]).catch(...)` for graceful degradation
5. Update the corresponding `+page.svelte` to use the new `PageData` shape from `./$types`
