# Legacy Deletion Next Wave

This note tracks cleanup that should come after the current deletion wave. The
project is pre-1.0 and should prefer the current model over compatibility shims,
but durable signed data still needs a deletion path that preserves verification.

## Deleted In The Current Wave

- `objects::delta` and `objects::store::compression` re-export modules were
  removed. Callers now import canonical `heddle_format::delta` and
  `heddle_format::compression` APIs directly.
- The raw exit-code string sentinels in `crates/cli/src/exit.rs` were removed.
  Exit classification now depends on typed `RecoveryAdvice`,
  `HeddleError::Recovery`, `HeddleError::Config`, and `RemoteError::NotFound`
  values.
- The dirty-worktree full-rematerialize refusal now returns typed
  `RecoveryDetails` instead of relying on rendered message text for exit-code
  behavior.
- The local daemon single-line pidfile fallback was removed. Probe logic now
  accepts only the structured `daemon::local_daemon::PidFileContents` format.
- The public diff schema title and generated TypeScript interface were renamed
  from `DiffOutput` to `DiffReport`.

Lane 6 registered the first deletion-prep migrations in
`crates/repo/src/migration.rs`:

- `0002_canonicalize_thread_records`
- `0003_canonicalize_context_roots`
- `0004_resecure_pre_fidelity_signatures`

These migrations are durable gates, not fallback deletion by themselves. Runtime
fallback removal still requires a later pass that proves every supported repo has
either applied the migration cleanly or has no fallback-dependent data left.

## Blocked On A Deliberate Contract Decision

### Legacy context direct-path fallback

Context file targets now write the canonical `__files/<path>` layout, and writes
clean up a matching legacy direct-path leaf. The read fallback remains because
`State.context` participates in `State::compute_hash()`, so rewriting historical
states from direct-path roots to canonical roots changes author-signature input.

Registered migration: `0003_canonicalize_context_roots`.

Current migration behavior:
- Rewrites unsigned states and locally owned signed states from direct-path
  context roots to canonical roots. For signed states, it computes the old hash
  candidates before rewriting and calls `Repository::resign_if_owned` after the
  new root is attached.
- Preserves signed states whose key is not owned by this repo and fails the
  migration gate instead of recording the migration as applied.
- Only after the migration reports zero fallback-dependent states may
  `lookup_context_leaf_for_target` and `context_target_from_entry_path` drop the
  direct-path fallback.

Deletion-prep tests:
- `repository_context::tests::legacy_direct_context_cannot_be_canonicalized_without_signature_decision`
  signs a state that points at a legacy direct-path context root and proves a
  naive canonical-root rewrite invalidates the signature.
- `migration::tests::migration_0003_canonicalizes_owned_signed_context_root_and_resigns`
  proves the registered migration rewrites and re-signs locally owned signed
  context roots.

Verification before deletion:
- Signed-state fixture with a locally owned legacy context root re-signs and
  verifies after migration.
- Foreign signed legacy context root is either preserved and reported or rejected
  by the chosen contract.
- `cargo test -p heddle-repo repository_context::tests:: -- --nocapture`
- `cargo test -p heddle-repo repository_signing::tests:: -- --nocapture`

### Thread-record serde defaults

Thread metadata still carries serde defaults for fields such as execution mode
and shared-target metadata. These are durable records, not just CLI aliases.

Registered migration: `0002_canonicalize_thread_records`.

Current migration behavior:
- Loads every thread record while the defaults still exist, then saves it back
  through `ThreadManager`.
- The migration must be idempotent and should not invent values for genuinely
  unknown optional metadata; it should persist the current semantic defaults
  (`freshness = unknown`, `auto = false`, empty path/impact vectors, empty
  summaries, `shared_target_dir = None`, etc.).
- After migration, add or invert an old-record fixture proving `Repository::open`
  no longer relies on `ThreadRecord`/`Thread` serde defaults.

Deletion-prep tests:
- `thread_storage::tests::thread_record_defaults_keep_minimal_legacy_shape_readable`
  documents the smallest legacy shape the current reader still accepts until the
  fallback-removal pass deletes serde defaults.
- `migration::tests::migration_0002_rewrites_minimal_thread_record_with_concrete_defaults`
  opens and migrates the legacy shape, then asserts the rewritten file contains
  the current concrete defaults.

Verification:
- `cargo test -p heddle-repo thread_storage::tests::thread_record_defaults_keep_minimal_legacy_shape_readable -- --nocapture`
- `cargo test -p heddle-repo migration::tests:: -- --nocapture`
- `cargo test -p heddle-cli --test multi_agent_worktrees -- --nocapture`
- `cargo test -p heddle-cli --test cli_integration thread -- --nocapture`

### Pre-fidelity state-signature compatibility

`State::compute_hash_pre_fidelity()` is still needed to verify states signed
before the git-fidelity hash bump.

Registered migration: `0004_resecure_pre_fidelity_signatures`.

Current migration behavior:
- Scans signed states, verifies the existing signature against both the current
  hash and `compute_hash_pre_fidelity()`, and re-signs only when
  `Repository::resign_if_owned` reports `Resigned`.
- Unsigned states remain unsigned; they do not need compatibility handling.
- Foreign valid pre-fidelity signatures are preserved and fail the migration
  gate instead of being laundered into this repo's identity.
- Only after the backfill proves no state needs the pre-fidelity candidate may
  `State::compute_hash_pre_fidelity()` and the `resign_if_owned` old-hash
  candidate path be removed.

Deletion-prep tests:
- `repository_signing::tests::resign_if_owned_accepts_legacy_pre_fidelity_signature`
  covers the locally owned re-sign path.
- `repository_signing::tests::resign_if_owned_refuses_foreign_pre_fidelity_signature`
  covers the preserve/reject path for valid signatures from keys this repo does
  not control.
- `repository_signing::tests::resign_if_owned_refuses_corrupted_pre_fidelity_signature`
  covers the no-laundering path for owned-key signatures that do not verify.
- Existing unsigned-state coverage remains
  `repository_signing::tests::resign_if_owned_reports_unsigned`.
- `migration::tests::migration_0004_resigns_owned_pre_fidelity_signature`
  proves the registered migration re-signs owned legacy signatures.
- `migration::tests::migration_0004_refuses_to_mark_foreign_pre_fidelity_signature_complete`
  proves foreign valid pre-fidelity signatures block the migration ledger.

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
