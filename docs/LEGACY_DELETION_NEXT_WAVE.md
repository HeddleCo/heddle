# Legacy Deletion Next Wave

This note tracks cleanup that should come after the current deletion wave. The
project is pre-1.0 and should prefer the current model over compatibility shims,
but durable signed data still needs a deletion path that preserves verification.

## Ready After Mechanical Migration

### `objects::delta` and `objects::store::compression` re-export shims

Current imports are mostly benchmarks and internal call sites that can point at
the canonical `heddle-format` modules instead.

Plan:
- Rewrite benchmark imports to use `heddle_format::delta` and
  `heddle_format::compression` directly.
- Rewrite any remaining internal imports to the canonical module.
- Delete the old re-export modules once `rg "objects::delta|store::compression"`
  is empty outside changelog/docs.

Verification:
- `cargo test -p heddle-objects --lib`
- `cargo check --benches -p heddle-objects`
- `cargo check --benches -p heddle-mount`

### Remaining raw exit-code sentinels

`HeddleError::Recovery` and typed CLI recovery envelopes now give this a better
home than string matching.

Plan:
- Inventory `crates/cli/src/exit.rs` string classification cases.
- Move durable cases to typed errors at their source.
- Keep message text strictly as rendering, not control flow.

Verification:
- `cargo test -p heddle-cli --lib exit -- --nocapture`
- `cargo test -p heddle-cli --test cli_integration output_kind_invariant -- --nocapture`

## Blocked On A Deliberate Contract Decision

### Legacy context direct-path fallback

Context file targets now write the canonical `__files/<path>` layout, and writes
clean up a matching legacy direct-path leaf. The read fallback remains because
`State.context` participates in `State::compute_hash()`, so rewriting historical
states from direct-path roots to canonical roots changes author-signature input.

Deletion requires one of these decisions:
- Provide an explicit migration that only rewrites unsigned states and locally
  owned signatures using `Repository::resign_if_owned`, while reporting foreign
  signed states that must stay on the fallback path.
- Or decide that pre-1.0 repositories with foreign signed legacy context roots
  may be rejected and document the break loudly.

Verification before deletion:
- Signed-state fixture with a locally owned legacy context root re-signs and
  verifies after migration.
- Foreign signed legacy context root is either preserved and reported or rejected
  by the chosen contract.
- `cargo test -p heddle-repo repository_context::tests:: -- --nocapture`
- `cargo test -p heddle-repo repository_signing::tests:: -- --nocapture`

### `DiffOutput` public schema title

The Rust type is now `DiffReport`, but the JSON schema title still reports
`DiffOutput` to avoid an accidental public-schema rename during cleanup.

Deletion/rename requires:
- Decide whether schema titles are public for pre-1.0 generated docs.
- If not public, rename the schema title to `DiffReport` and regenerate schema
  docs.
- If public, keep the old title until the next explicit schema compatibility
  break.

Verification:
- `cargo test -p heddle-core diff::patch -- --nocapture`
- `cargo test -p heddle-cli --test cli_integration diff_patch_conformance -- --nocapture`
- `cargo test -p heddle-cli --test cli_integration output_kind_invariant -- --nocapture`

### Thread-record serde defaults

Thread metadata still carries serde defaults for fields such as execution mode
and shared-target metadata. These are durable records, not just CLI aliases.

Deletion requires:
- A repository migration that rewrites every thread record to the current shape.
- A fixture with an old record proving `Repository::open` no longer needs the
  defaults.

Verification:
- `cargo test -p heddle-cli --test multi_agent_worktrees -- --nocapture`
- `cargo test -p heddle-cli --test cli_integration thread -- --nocapture`

### Pre-fidelity state-signature compatibility

`State::compute_hash_pre_fidelity()` is still needed to verify states signed
before the git-fidelity hash bump.

Deletion requires:
- A backfill that re-signs locally owned legacy states or a decision to reject
  those signatures.
- Fixtures for locally owned, unsigned, foreign signed, and corrupted legacy
  states.

Verification:
- `cargo test -p heddle-repo repository_signing::tests:: -- --nocapture`
- `cargo test -p heddle-cli --test cli_integration verify -- --nocapture`

## Product Or Sley-Gated Cleanup

### Legacy gitlink blob convention

The `heddle-submodule:` blob convention is a bridge compromise. Deleting it
needs a replacement import/export story that preserves Git submodule semantics.

Verification:
- Git bridge round-trip tests for submodules.
- Diff patch conformance against Git worktrees containing gitlinks.

### Hidden maintenance commands

Hidden `index` and `monitor` surfaces should either graduate into supported
maintenance UX or be deleted. This is a product decision, not a mechanical
compatibility cleanup.

Verification:
- Command catalog visibility tests.
- `cargo test -p heddle-cli --lib command_catalog -- --nocapture`

### Sley-gated Git pack and remote seams

Do not replace remaining Git pack/remote gaps with Heddle-local plumbing. Delete
Heddle-side compensating code only after Sley exposes the corresponding facade.

Verification:
- Sley parity tests for the facade.
- Heddle hosted/local sync targeted tests after dependency update.
