# Legacy Deletion Next Wave

This note tracks cleanup that should come after the current deletion wave. The
project is pre-1.0 and should prefer the current model over compatibility shims,
but durable signed data still needs a deletion path that preserves verification.

Active cleanup plan: `docs/VERIFICATION_CLEANUP_PLAN.md` tracks the staged
removal of duplicate CLI/core verification ownership and stale bridge-mirror
language. The null-only plain-Git import hint sidecar has been deleted, and
public `status` / `doctor` JSON no longer emits legacy
`git_overlay_import_hint` / `git_overlay_health` sidecars. Remaining work is
core/CLI verification ownership and internal naming cleanup.

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
  `0004_canonicalize_context_roots`.
- Top-level `ThreadRecord` serde defaults were removed from the live durable
  reader. Old minimal TOML is parsed only by the private, migration-only
  `LegacyThreadRecord` used by `0002_canonicalize_thread_records`, then
  rewritten as current canonical TOML.
- The pre-fidelity state hash is no longer a normal-looking public API.
  `State::compute_hash_for_legacy_signature_migration()` is hidden and owned by
  `0005`; ordinary signing tests no longer exercise it as live behavior.
- `PackedOpLog::load` is current-format only. V2/V3 containers and old
  OpRecord schemas decode only through `PackedOpLog::ensure_latest`, which is
  now driven by registered migration `0006_canonicalize_packed_oplog`.
- The deprecated ignore-driven worktree descendant remover was deleted.
  `revert` now uses the tree-driven tracked-removal interface when it can prove
  the target state contains a subtree for the path, and otherwise refuses the
  unexpected file-vs-directory mismatch instead of recursing by current ignore
  rules.
- The current Git command projection path now lives under `git_projection` internals. User-facing `commit` / `switch` behavior is
  unchanged; the code no longer looks like a legacy compatibility shim.
- `maintenance index` and `maintenance monitor` graduated from hidden clap
  subcommands to discoverable admin maintenance commands. Their machine
  contracts were already cataloged under the admin surface.

Lane 6 registered the first deletion-prep migrations in
`crates/repo/src/migration.rs`:

- `0002_canonicalize_thread_records`
- `0003_canonicalize_tree_entries`
- `0004_canonicalize_context_roots`
- `0005_resecure_pre_fidelity_signatures`
- `0006_canonicalize_packed_oplog`

These migrations are durable gates. The current deletion pass moved the
remaining old readers behind those gates rather than leaving them mixed into
normal runtime code.

## Migration-Gated Compatibility Now Localized

### Legacy context direct-path fallback

Context file targets now read and write only the canonical `__files/<path>`
layout. The direct-path fallback is migration-only because `State.context`
participates in `State::compute_hash()`, so rewriting historical states from
direct-path roots to canonical roots changes author-signature input.

Registered migration: `0004_canonicalize_context_roots`.

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
records are parsed only by the private, migration-only `LegacyThreadRecord`.

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

Registered migration: `0005_resecure_pre_fidelity_signatures`.

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
- `migration::tests::migration_0005_resigns_owned_pre_fidelity_signature`
  proves the registered migration re-signs owned legacy signatures.
- `migration::tests::migration_0005_refuses_to_mark_foreign_pre_fidelity_signature_complete`
  proves foreign valid pre-fidelity signatures block the migration ledger.

Verification:
- `cargo test -p heddle-repo repository_signing::tests:: -- --nocapture`
- `cargo test -p heddle-cli --test cli_integration verify -- --nocapture`

### Old packed-oplog schemas

Normal packed-oplog loads now accept only latest container + current OpRecord
schema. Old V2/V3 containers and old per-record schemas remain decodable only
through `PackedOpLog::ensure_latest`.

Registered migration: `0006_canonicalize_packed_oplog`.

Verification:
- `cargo test -p heddle-oplog packed_oplog -- --nocapture`
- `cargo test -p heddle-repo migration::tests:: -- --nocapture`

## Product Or Sley-Gated Cleanup

### Legacy gitlink blob convention

The `heddle-submodule:` blob convention was a bridge compromise. Runtime
import/export no longer uses it; the remaining allowance is migration-only
decoding of old stores. The settled target model is in
[`ADR-0041`](adr/0041-first-class-gitlinks.md): Gitlinks are first-class tree
entry targets whose value is a format-aware Sley Git object id.

Removed runtime writers/readers:
- `crates/ingest/src/importer.rs` now writes first-class Gitlink tree entries.
- `crates/cli/src/cli/commands/git_projection.rs` now writes first-class Gitlink
  entries for Git-index gitlinks.
- `crates/git-projection/src/git_export.rs` now emits Git gitlinks only from
  first-class Gitlink targets and never sniffs ordinary blob content.

Completed deletion direction:
- Replaced the flat `TreeEntry { name, mode, entry_type, hash }` shape with a
  closed target model: blob, tree, symlink, or gitlink.
- Added explicit V2 durable tree encoding with stable target tags.
- Moved import/export paths to first-class Gitlink targets.
- Moved the old marker parser out of general utilities into
  `objects::legacy::decode_gitlink_blob_marker`.
- Registered `0003_canonicalize_tree_entries` as the hard tree-format gate.
  It decodes the old unversioned tree shape only inside the migration and
  rewrites it as current V2 tree bytes at the same semantic tree hash.
- Bumped the repo-level format gate to 2 so older binaries refuse migrated
  repositories cleanly.
- Fixed GC pack/prune so migrated loose V2 tree shadows are not erased in
  favor of older packed V1 bodies with the same semantic tree hash.

Remaining migration direction:
- Convert legacy magic blobs only when Sley can prove the original mapped Git
  tree had mode `160000` at the same path with the same target OID. Preserve
  ambiguous magic blobs as ordinary files and report
  `ambiguous_legacy_gitlink_blob` from migration/verify/fsck.
- Preserve logical `ChangeId` while recomputing affected tree/state hashes.
  Re-sign locally owned rewritten states with the existing `resign_if_owned`
  path and refuse foreign signed states that require rewrite.

Verification:
- Golden V1 and V2 tree fixtures.
- Proven legacy gitlink migration and ambiguous magic-prefix-file migration
  tests.
- Git Projection round-trip tests for submodules.
- Diff patch conformance against Git worktrees containing gitlinks.
- Export test proving the Gitlink target object does not need to exist in the
  superproject object database.
- Object traversal/pack/wire tests proving Gitlink targets are not treated as
  Heddle object dependencies.

### Hidden maintenance commands

`maintenance index` and `maintenance monitor` have graduated into the admin
maintenance surface. The remaining cleanup is contract polish, not deletion:
`maintenance monitor` still has an opaque schema in the command catalog.

Verification:
- Command catalog visibility tests.
- `cargo test -p heddle-cli --lib command_catalog -- --nocapture`

### Sley-gated Git pack and remote seams

Do not replace remaining Git pack/remote gaps with Heddle-local plumbing. Delete
Heddle-side compensating code only after Sley exposes the corresponding facade.

Verification:
- Sley parity tests for the facade.
- Heddle hosted/local sync targeted tests after dependency update.

### Raw Git Object Residuals + Bridge Mirror retirement (foundation)

Foundation is in place; the persistent Bridge Mirror (`.heddle/git`) is **not**
deleted yet.

Shipped foundation:
- Durable residual store at `.heddle/git-residuals/<format>/<oid-prefix>/<oid-rest>`
  (`heddle_git_projection::ResidualStore`): put/get/has/list, hash-identity
  verification, lazy migrate-from-mirror helper.
- Checkout materialize / export lossy paths prefer reconstruct, then residual,
  then Bridge Mirror; hard-fail when a mapped non-reconstructable object has
  neither residual nor mirror bytes.
- Maintenance inspection via `bridge_mirror_retirement_status` (report only;
  no mirror deletion).
- `init_mirror` remains for migration paths.

Not yet complete (do not claim done):
- Full residual capture on lossy import for entire tree/blob closures.
- Fsck residual validation for every mapped non-reconstructable oid.
- Explicit maintenance command that deletes an empty migrated Bridge Mirror.
- Export/push/sync that never opens `.heddle/git` at all.

See `docs/adr/0042-retire-persistent-bridge-mirror.md` and
`docs/VERIFICATION_CLEANUP_PLAN.md` (Track B).
