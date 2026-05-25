# Contributing to Heddle

Heddle is alpha software with strong opinions about its CLI surface, its
output contracts, and its render discipline. This file is the on-ramp:
read it, follow the reading order, and your first PR will land cleanly.

## Contributing across HeddleCo

Heddle the CLI is one of three repos. The hosted server
([weft](https://github.com/HeddleCo/weft)) and the web app
([tapestry](https://github.com/HeddleCo/tapestry)) sit alongside it,
and a single identity + permission model governs participation across
all three. The substrate and the three grant flows are designed in
[`docs/spikes/contribution-grant-flows.md`](docs/spikes/contribution-grant-flows.md);
that doc is the source of truth for any claim made here.

### What you can do without an account

Anonymous callers (anon biscuit only, per spike §2) can **read public
state**: clone public repos, view public reviews, browse the docs
site. They cannot sign reviews, post discussion turns, comment, or
otherwise mutate anything. The substrate gates writes via
`require_user_subject` (spike §2) — anon callers receive
`failed_precondition` from every write RPC.

### Getting an authorized identity

Writes require a hosted account. Sign up through the tapestry
frontend; the flow uses passkey registration plus email
verification (the hardened path tracked under
`HeddleCo/weft#181`). After signup you have a hosted account but no
repo permissions yet — that's the next step.

### Getting repo permissions

Three flows produce the same `(subject_user_uuid, resource, role)`
substrate triple (spike §3):

1. **Maintainer-initiated invite.** A maintainer (>= `Admin` on the
   target repo or any ancestor namespace) invites you from the repo
   or namespace settings page, by handle or by email. Handle →
   `CreateGrant`; email → `CreateInvitation` redeemed on signup.
2. **User-initiated request-to-contribute.** A signed-in user without
   a role on a public resource sees a "Request access" affordance
   and submits a justification. A maintainer reviews from the
   pending-requests inbox and approves (optionally counter-offering
   a lower role) or denies. Spec lives in spike §3.2; the RPCs
   (`GetMyEffectiveRole`, `RequestRoleGrant`,
   `ListPendingGrantRequests`, `RespondToGrantRequest`) are the
   sub-impl batch listed in spike §5 — **planned, not yet shipped**.
3. **Namespace-level inheritance.** A grant on a parent namespace
   (e.g. `org/acme`) flows down to its child repos automatically.
   The substrate already walks ancestors via
   `effective_role_for_repository` (spike §1); the repo-settings UI
   surfaces direct and inherited grants in separate sections (spike
   §3.3, also a planned sub-impl).

There is **no "guest contributor" tier**. You are either anonymous
(read-only-public) or you have a hosted account plus a role grant.
That's the entire model.

### What you can do once you have a role

Roles are ordinal — `Reader < Developer < Maintainer < Admin <
Owner` (defined in `weft/crates/weft-server/src/access/enforce.rs:5`;
12-capability enumeration in `access/scope.rs:34-48`). The role you
hold on a repo (direct or inherited) determines which RPCs are
admitted on its resources. See spike §1 for the capability table; the
short version is that higher roles strictly add capabilities to the
lower roles below them.

### The PR shape

- Link the issue you're closing (`Closes HeddleCo/<repo>#<n>`).
- For Rust changes, follow the red-commit-first discipline: failing
  test, then implementation. Note the red commit SHA in the PR body.
- Match the repo's coverage rules (see this file's "Coverage gate"
  section for heddle; equivalents in the other repos).
- Run the repo's local CI parity commands before pushing — listed in
  this file's "PR checklist" section.

### Where to ask questions

- Bugs and regressions: GitHub Issues on the relevant repo.
- Quick how-to: GitHub Discussions on `HeddleCo/heddle`.
- Larger design questions: draft a markdown doc under `docs/` and
  reference it from the issue or PR.

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

When your change touches a stable surface (a public API on one of the
crates listed in [docs/STABILITY.md](docs/STABILITY.md) §2.1, a format-
version constant under `crates/objects/src/object/`, the oplog or
ref-summary version, the `heddle.v1` proto package, or a Tier-`Everyday`
CLI verb / flag), read [docs/STABILITY.md](docs/STABILITY.md) first.
The document is currently a **strawman under maintainer review** — its
`<TBD: maintainer>` placeholders are policy decisions still being
finalized — so treat it as proposed-and-likely guidance rather than
binding rules until those markers are resolved. The SemVer and
deprecation framing it proposes is the direction we expect to ship,
and aligning to it now will avoid churn when it's ratified.

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

## Coverage gate

The `Coverage` job in
[`.github/workflows/rust-tests.yml`](.github/workflows/rust-tests.yml)
runs `cargo llvm-cov` over the OSS feature set and then enforces a
per-crate line-coverage floor on the resulting `lcov.info`. The gate
is the `Coverage gate (objects / repo / refs)` step; it shells out to
the `audit-coverage` subcommand of `heddle-devtools`:

```bash
cargo run -p heddle-devtools --quiet -- \
  audit-coverage lcov.info \
    --gate objects=80 \
    --gate repo=78.66 \
    --gate refs=80
```

`audit-coverage` parses `SF:` / `LF:` / `LH:` records in the lcov
output, aggregates by workspace crate (matched on
`crates/<name>/...`), and exits non-zero when any gated crate is
below its threshold. The CI step fails *before* the Codecov upload,
so the build stays red whether or not Codecov is reachable.

### Current thresholds

| Crate | Threshold | Goal |
|---|---|---|
| `objects` | 80% | 80% |
| `repo` | 78.66% | 80% (ratchet) |
| `refs` | 80% | 80% |

The `repo` threshold is a ratchet floor: current main is **78.66%**,
so the gate locks that as the no-regression line. Raise the number
in this table, in
[`.github/workflows/rust-tests.yml`](.github/workflows/rust-tests.yml),
and in [`codecov.yml`](codecov.yml) in the same PR that adds the
tests that push `repo`'s coverage to ≥80%.

### Codecov mirror

[`codecov.yml`](codecov.yml) declares `coverage.status.project.<crate>`
entries with the same `target:` values as the CI gate, so Codecov's
PR comment reports the same numbers the build enforces. Codecov is
not the gate of record — the in-CI step is — but the two must agree.
When you change a threshold, change it in all three places
(`rust-tests.yml`, `codecov.yml`, this table).

### Running the gate locally

```bash
cargo llvm-cov --locked --workspace \
  --features git-overlay,native,semantic,zstd \
  --lcov --output-path lcov.info

cargo run -p heddle-devtools --quiet -- \
  audit-coverage lcov.info \
    --gate objects=80 --gate repo=78.66 --gate refs=80
```

A green local run means a green CI run, modulo lcov's normal
sensitivity to feature flags. The `--features` list above mirrors
the one the CI `Coverage` job uses (see
[`.github/workflows/rust-tests.yml`](.github/workflows/rust-tests.yml)
— the comment above the `Generate coverage report` step explains why
this set, not `--all-features`).

## Getting unstuck

- Quick how-to questions: open a GitHub Discussion.
- Bugs and regressions: open a GitHub Issue with a minimal reproduction
  recipe (`heddle init && …`). The CLI's `--output json` form is a
  reliable way to attach machine-readable repro state.
- Larger design questions (new verb, new schema field, new principle):
  start with a draft markdown doc in `docs/` and reference it from the
  PR.
