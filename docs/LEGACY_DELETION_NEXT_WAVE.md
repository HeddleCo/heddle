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

Migration hooks for this wave are reserved, but intentionally not registered, in
`crates/repo/src/migration.rs` as `NEXT_DELETION_WAVE_MIGRATIONS`. Registering
one of them means the migration body exists, the safety gate in that hook is
satisfied, and the tests named below have been inverted or extended to prove the
runtime fallback is no longer needed.

## Blocked On A Deliberate Contract Decision

### Legacy context direct-path fallback

Context file targets now write the canonical `__files/<path>` layout, and writes
clean up a matching legacy direct-path leaf. The read fallback remains because
`State.context` participates in `State::compute_hash()`, so rewriting historical
states from direct-path roots to canonical roots changes author-signature input.

Prepared hook: `0003_canonicalize_context_roots`.

Deletion requires an explicit contract:
- Safe migration path: rewrite unsigned states and locally owned signed states
  from direct-path context roots to canonical roots. For signed states, compute
  the old hash candidates before rewriting and call `Repository::resign_if_owned`
  after the new root is attached.
- Foreign or corrupted signed states must not be rewritten silently. The
  migration must either preserve them and report "still needs legacy fallback",
  or reject the repo with an explicit pre-1.0 break message.
- Only after the migration reports zero fallback-dependent states may
  `lookup_context_leaf_for_target` and `context_target_from_entry_path` drop the
  direct-path fallback.

Prepared tests:
- `repository_context::tests::legacy_direct_context_cannot_be_canonicalized_without_signature_decision`
  signs a state that points at a legacy direct-path context root and proves a
  naive canonical-root rewrite invalidates the signature.

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

Prepared hook: `0002_canonicalize_thread_records`.

Deletion requires:
- A repository migration that loads every thread record while the defaults still
  exist, then saves it back in the full current shape through `ThreadManager`.
- The migration must be idempotent and should not invent values for genuinely
  unknown optional metadata; it should persist the current semantic defaults
  (`freshness = unknown`, `auto = false`, empty path/impact vectors, empty
  summaries, `shared_target_dir = None`, etc.).
- After migration, add or invert an old-record fixture proving `Repository::open`
  no longer relies on `ThreadRecord`/`Thread` serde defaults.

Prepared tests:
- `thread_storage::tests::thread_record_defaults_keep_minimal_legacy_shape_readable`
  documents the smallest legacy shape the current reader still accepts. The
  deletion commit should invert this fixture: first open+migrate the legacy
  shape, then assert the rewritten file contains the current complete shape.

Verification:
- `cargo test -p heddle-repo thread_storage::tests::thread_record_defaults_keep_minimal_legacy_shape_readable -- --nocapture`
- `cargo test -p heddle-repo migration::tests:: -- --nocapture`
- `cargo test -p heddle-cli --test multi_agent_worktrees -- --nocapture`
- `cargo test -p heddle-cli --test cli_integration thread -- --nocapture`

### Pre-fidelity state-signature compatibility

`State::compute_hash_pre_fidelity()` is still needed to verify states signed
before the git-fidelity hash bump.

Prepared hook: `0004_resecure_pre_fidelity_signatures`.

Deletion requires:
- A backfill that scans legacy signed states, verifies the existing signature
  against both the current hash and `compute_hash_pre_fidelity()`, and re-signs
  only when `Repository::resign_if_owned` reports `Resigned`.
- Unsigned states remain unsigned; they do not need compatibility handling.
- Foreign signed and corrupted signed states must not be laundered into this
  repo's identity. The migration must either preserve/report them or reject the
  repo under an explicit pre-1.0 contract.
- Only after the backfill proves no state needs the pre-fidelity candidate may
  `State::compute_hash_pre_fidelity()` and the `resign_if_owned` old-hash
  candidate path be removed.

Prepared tests:
- `repository_signing::tests::resign_if_owned_accepts_legacy_pre_fidelity_signature`
  covers the locally owned re-sign path.
- `repository_signing::tests::resign_if_owned_refuses_foreign_pre_fidelity_signature`
  covers the preserve/reject path for valid signatures from keys this repo does
  not control.
- `repository_signing::tests::resign_if_owned_refuses_corrupted_pre_fidelity_signature`
  covers the no-laundering path for owned-key signatures that do not verify.
- Existing unsigned-state coverage remains
  `repository_signing::tests::resign_if_owned_reports_unsigned`.

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
