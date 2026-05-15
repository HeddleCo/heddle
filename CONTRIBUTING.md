# Contributing to Heddle

Heddle is alpha software with strong opinions about its CLI surface, its
output contracts, and its render discipline. This file is the on-ramp:
read it, follow the reading order, and your first PR will land cleanly.

## Reading order

Three files, in order, before touching code that changes behavior or
public surface:

1. **[AGENTS.md](AGENTS.md)** — contributor rules, scope boundaries, what
   docs are authoritative for which behavior.
2. **[docs/PRINCIPLES.md](docs/PRINCIPLES.md)** — the five operating
   principles (trust, disposability, composability, restraint, honesty).
   Every CLI change is graded against these. "If a change makes one of
   them weaker, the change is wrong."
3. **[CLAUDE.md](CLAUDE.md)** — repo-level Claude Code hooks and the
   Heddle-dogfooding workflow. Includes the four known caveats around
   verify-hook fail-closed, attribution propagation, `heddle start`
   cwd, and Stop-hook timing.

## Build and test

```bash
cargo build                        # debug build
cargo build --release              # release build
cargo test                         # full workspace
cargo test -- --nocapture          # see println / dbg output
cargo clippy -- -D warnings        # lint with warnings-as-errors
cargo fmt --check                  # formatting gate
```

For the web app:

```bash
cd web && bun install
cd web && bun run dev              # SvelteKit dev server
cd web && npx svelte-check         # type-check
```

## Tests new contributors meet first

These four meta-tests enforce contracts that aren't enforced by `cargo
build`. Expect to update one of them with most non-trivial PRs.

- **`crates/cli/tests/render_lint.rs`** — counts `println!` / `print!`
  invocations outside `render_*` / `write_*` functions in
  `crates/cli/src/cli/commands/`. The constant
  `RENDER_VIOLATION_BASELINE` is a hard ceiling: removing violations is
  always safe, but adding any without lowering the constant by the
  matching count breaks CI. The discipline (and the per-file punch list)
  lives in
  [crates/cli/src/cli/commands/RENDER_AUDIT.md](crates/cli/src/cli/commands/RENDER_AUDIT.md).
- **`crates/cli/tests/tier_coverage.rs`** — enumerates every `Commands`
  enum variant and requires it to be classified as `Everyday`,
  `Advanced`, or `Hidden`. Adding a verb without claiming a tier fails
  the test.
- **`crates/cli/tests/op_id_coverage.rs`** — every state-changing verb
  must thread the `--op-id` flag through to its dedup-store callsite.
  The lint catches verbs that forgot to opt into idempotent retries.
- **`crates/cli/tests/idempotency_lint.rs`** — verifies the verbs claimed
  in `op_id_coverage` actually behave idempotently when the same op-id
  is replayed.

Two diagnostic verbs ride alongside the tests as part of the contract:

- **`heddle doctor docs --all`** — walks every `heddle <verb>`
  invocation in tracked markdown and reports drift against the live
  binary. New flag names referenced from docs must be real flags before
  the PR can land cleanly.
- **`heddle doctor schemas`** — validates the JSON samples in
  [docs/json-schemas.md](docs/json-schemas.md) against the runtime
  schema registry. The 21 registered schemas are output-shape
  contracts; changes must be additive (`null` for missing fields,
  empty arrays explicit).

## Render discipline

`println!` and `print!` are reserved for functions named `render_*` or
`write_*` (or inside `#[cfg(test)]`). Stderr is unrestricted — warnings
and tips ride there. The lint is a ratchet, not a contract: the
baseline goes down, never up. See
[crates/cli/src/cli/commands/RENDER_AUDIT.md](crates/cli/src/cli/commands/RENDER_AUDIT.md)
for the per-file status and which files are next on the chip list.

When you add a verb's user-facing output, mirror the canonical
pattern in `log.rs` or `diff/diff_output.rs`: build a `*Output`
struct, derive `Serialize`, and write a `render_<verb>` function that
takes that struct. The same struct serializes to JSON via
`serde_json::to_string`; selection is decided by
`cli::cli::should_output_json(cli, Some(repo.config()))`.

## Short-flag conventions

Documented in
[crates/cli/src/cli/cli_args/cli_base.rs](crates/cli/src/cli/cli_args/cli_base.rs)
at the top of the file. The table is authoritative — new short flags
reuse the existing letters where the semantic matches (`-m` for
message, `-f` for force, `-n` for "how many"). Renames are out of
scope; scripts written against the surface MUST keep working.

## PR checklist

Before opening a PR, run:

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
heddle doctor docs
heddle doctor schemas
```

When the change touches the CLI surface (a new verb, a new flag, a new
output shape), also run:

```bash
heddle <verb> --help          # eyeball clap's rendering
```

If you added or moved an entry in `Commands`, expect to update
`tier_coverage`. If you added a state-changing verb, expect to update
`op_id_coverage` and `idempotency_lint`. If you added user-facing
output, you'll either route it through `render_*` (no baseline
change) or accept that you need to lower `RENDER_VIOLATION_BASELINE`
by the count you removed elsewhere.

## The principles

From [docs/PRINCIPLES.md](docs/PRINCIPLES.md):

> The surface is small on purpose, the outputs are honest on purpose,
> and the verbs compose because the primitives beneath them are the
> right shape. Five principles run through every command: trust,
> disposability, composability, restraint, honesty. Read this before
> you add a verb, change a flag, or argue for a new output field.

If a proposed change makes any of those weaker — even slightly — the
change is wrong. Argue from the principles, not around them.

## Security

Heddle gates every PR (and runs a weekly cron) through a supply-chain
audit. The gate is enforced by
[`.github/workflows/audit.yml`](.github/workflows/audit.yml) and lives
across two tools:

- **cargo-deny** — reads [`deny.toml`](deny.toml). Enforces the license
  allow-list, the banned-crates list (e.g. `openssl` / `openssl-sys` /
  `native-tls` — Heddle standardizes on rustls), the registry/git source
  allow-list, and the RustSec advisory database.
- **cargo-audit** — second-opinion advisory scan against the same
  RustSec DB, run with `--deny warnings` so yanked / unmaintained /
  notice advisories also fail the build.

### Running the gate locally

```bash
cargo install --locked cargo-deny  # once
cargo install --locked cargo-audit # once

cargo deny check                   # full policy run

# advisory check — flags MUST mirror .github/workflows/audit.yml so a green
# local run means a green CI run. If you add/remove an --ignore here, also
# update the workflow (cargo-audit doesn't read deny.toml's ignore list).
cargo audit \
  --deny warnings \
  --ignore RUSTSEC-2023-0071 \
  --ignore RUSTSEC-2026-0098 \
  --ignore RUSTSEC-2026-0099 \
  --ignore RUSTSEC-2026-0104
```

Both should be green before you push. CI runs them on every PR (no
paths filter — see `audit.yml` for why), every push to `main`, and on a
weekly schedule (Mondays 05:17 UTC) so a fresh advisory against an
unchanged dependency surfaces without anyone having to push code.

### Ignoring an advisory

If a RustSec advisory cannot be fixed by a version bump (upstream
hasn't released; advisory doesn't apply to our usage; etc.), add an
entry to **all three** of:

1. `deny.toml` — `[advisories].ignore` with `{ id, reason }`. The
   `reason` must explain *why* it's safe to ignore in Heddle's context,
   not just acknowledge the advisory exists.
2. `.github/workflows/audit.yml` — `cargo audit --ignore <ID>` in the
   `cargo-audit` step. cargo-audit doesn't read `deny.toml`, so the
   two lists must be kept in sync by hand.
3. The local-run command in this file (above) — so contributors running
   `cargo audit` locally see the same advisories pass that CI does.

When upstream ships a fix, remove the entry from all three places in
the same PR that bumps the dependency.

### Adding a license to the allow-list

Adding a license is a policy decision. Discuss in the PR before
extending `[licenses].allow` in `deny.toml`. Copyleft licenses
(GPL-*, AGPL-*, LGPL-*) are not on the allow-list by design and
should not be added without sign-off — Heddle ships under Apache-2.0
and we don't want copyleft obligations leaking into binaries.

### Lifting a crate ban

Same shape as license additions: discuss first. The banned list in
`[bans].deny` reflects intentional architectural choices (rustls only,
no OpenSSL coupling), not arbitrary preferences. If you have a real
need, the PR should explain why the alternative isn't viable.

## Getting unstuck

- Quick how-to questions: open a GitHub Discussion.
- Bugs and regressions: open a GitHub Issue with a minimal reproduction
  recipe (`heddle init && …`). The CLI's `--output json` form is a
  reliable way to attach machine-readable repro state.
- Larger design questions (new verb, new schema field, new principle):
  start with a draft markdown doc in `docs/` and reference it from the
  PR.
