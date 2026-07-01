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
- Legacy direct-path context reads were removed from the normal context API.
  `Repository::get_context_blob`, `list_context_entries`, `set_context_blob`,
  and `remove_context_target` now operate on canonical `__files/<path>` /
  `__states/<id>` entries only. The direct-path reader is private to
  `0003_canonicalize_context_roots`.
- Top-level `ThreadRecord` serde defaults were removed from the live durable
  reader. Old minimal TOML is parsed only by the private `LegacyThreadRecord`
  used by `0002_canonicalize_thread_records`, then rewritten as current
  canonical TOML.
- The pre-fidelity state hash is no longer a normal-looking public API.
  `State::compute_hash_for_legacy_signature_migration()` is hidden and owned by
  `0003`/`0004`; ordinary signing tests no longer exercise it as live behavior.
- `PackedOpLog::load` is current-format only. V2/V3 containers and old
  OpRecord schemas decode only through `PackedOpLog::ensure_latest`, which is
  now driven by registered migration `0005_canonicalize_packed_oplog`.

Lane 6 registered the first deletion-prep migrations in
`crates/repo/src/migration.rs`:

- `0002_canonicalize_thread_records`
- `0003_canonicalize_context_roots`
- `0004_resecure_pre_fidelity_signatures`
- `0005_canonicalize_packed_oplog`

These migrations are durable gates. The current deletion pass moved the
remaining old readers behind those gates rather than leaving them mixed into
normal runtime code.

## Migration-Gated Compatibility Now Localized

### Legacy context direct-path fallback

Context file targets now read and write only the canonical `__files/<path>`
layout. The direct-path fallback is migration-only because `State.context`
participates in `State::compute_hash()`, so rewriting historical states from
direct-path roots to canonical roots changes author-signature input.

Registered migration: `0003_canonicalize_context_roots`.

Current migration behavior:
- Rewrites unsigned states and locally owned signed states from direct-path
  context roots to canonical roots. For signed states, it computes the old hash
  candidates before rewriting and calls `Repository::resign_if_owned` after the
  new root is attached.
- Preserves signed states whose key is not owned by this repo and fails the
  migration gate instead of recording the migration as applied.
- Normal reads no longer call the direct-path fallback; failed migrations leave
  old direct-path data intentionally invisible until it is migrated or handled
  by the owning key.

Deletion-prep tests:
- `repository_context::tests::direct_context_canonicalization_requires_signature_decision`
  signs a state that points at a legacy direct-path context root and proves a
  naive canonical-root rewrite invalidates the signature.
- `repository_context::tests::legacy_direct_file_context_is_migration_only`
  proves normal reads ignore direct-path leaves while the migration walker can
  still canonicalize them.
- `migration::tests::migration_0003_canonicalizes_owned_signed_context_root_and_resigns`
  proves the registered migration rewrites and re-signs locally owned signed
  context roots.

Verification:
- Signed-state fixture with a locally owned legacy context root re-signs and
  verifies after migration.
- Foreign signed legacy context root is either preserved and reported or rejected
  by the chosen contract.
- `cargo test -p heddle-repo repository_context::tests:: -- --nocapture`
- `cargo test -p heddle-repo repository_signing::tests:: -- --nocapture`

### Thread-record serde defaults

Live thread metadata now requires current canonical fields. Missing-field legacy
records are parsed only by the migration-local `LegacyThreadRecord`.

Registered migration: `0002_canonicalize_thread_records`.

Current migration behavior:
- Loads every thread record through the private legacy shape, then saves it back
  through `FilesystemThreadRecordStore` as a current `ThreadRecord`.
- The migration must be idempotent and should not invent values for genuinely
  unknown optional metadata; it should persist the current semantic defaults
  (`freshness = unknown`, `auto = false`, empty path/impact vectors, empty
  summaries, `shared_target_dir = None`, etc.).
- After migration, the live `ThreadRecord` reader rejects the old minimal
  missing-field fixture.

Deletion-prep tests:
- `thread_storage::tests::thread_record_reader_rejects_minimal_legacy_shape_after_migration_gate`
  documents that the live reader no longer accepts the smallest legacy shape.
- `migration::tests::migration_0002_rewrites_minimal_thread_record_with_concrete_defaults`
  opens and migrates the legacy shape, then asserts the rewritten file contains
  the current concrete defaults.

Verification:
- `cargo test -p heddle-repo thread_storage::tests::thread_record_reader_rejects_minimal_legacy_shape_after_migration_gate -- --nocapture`
- `cargo test -p heddle-repo migration::tests:: -- --nocapture`
- `cargo test -p heddle-cli --test multi_agent_worktrees -- --nocapture`
- `cargo test -p heddle-cli --test cli_integration thread -- --nocapture`

### Pre-fidelity state-signature compatibility

The legacy pre-fidelity state hash is now a hidden migration-only helper used to
verify states signed before the git-fidelity hash bump.

Registered migration: `0004_resecure_pre_fidelity_signatures`.

Current migration behavior:
- Scans signed states, verifies the existing signature against both the current
  hash and `compute_hash_for_legacy_signature_migration()`, and re-signs only
  when `Repository::resign_if_owned` reports `Resigned`.
- Unsigned states remain unsigned; they do not need compatibility handling.
- Foreign valid pre-fidelity signatures are preserved and fail the migration
  gate instead of being laundered into this repo's identity.
- Normal signing tests no longer exercise the pre-fidelity recipe; only the
  migration and golden-vector tests keep it alive.

Deletion-prep tests:
- Existing unsigned-state coverage remains
  `repository_signing::tests::resign_if_owned_reports_unsigned`.
- `migration::tests::migration_0004_resigns_owned_pre_fidelity_signature`
  proves the registered migration re-signs owned legacy signatures.
- `migration::tests::migration_0004_refuses_to_mark_foreign_pre_fidelity_signature_complete`
  proves foreign valid pre-fidelity signatures block the migration ledger.

Verification:
- `cargo test -p heddle-repo repository_signing::tests:: -- --nocapture`
- `cargo test -p heddle-cli --test cli_integration verify -- --nocapture`

### Old packed-oplog schemas

Normal packed-oplog loads now accept only latest container + current OpRecord
schema. Old V2/V3 containers and old per-record schemas remain decodable only
through `PackedOpLog::ensure_latest`.

Registered migration: `0005_canonicalize_packed_oplog`.

Verification:
- `cargo test -p heddle-oplog packed_oplog -- --nocapture`
- `cargo test -p heddle-repo migration::tests:: -- --nocapture`

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
