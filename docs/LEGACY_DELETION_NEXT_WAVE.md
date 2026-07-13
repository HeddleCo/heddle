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
  `__states/<id>` entries only. Repository format v3 does not carry the old
  layout forward.
- Top-level `ThreadRecord` serde defaults were removed from the live durable
  reader. Old minimal TOML is parsed only by the private, migration-only
  `LegacyThreadRecord` used by `0002_canonicalize_thread_records`, then
  rewritten as current canonical TOML.
- The pre-fidelity state-hash compatibility path was removed at the repository
  format v3 boundary.
- `PackedOpLog` accepts only the V4 container with the StateId-native OpRecord
  schema. V2/V3 containers and record schemas 1–3 are refused without being
  rewritten; repository format v3 is the history-format boundary.
- The deprecated ignore-driven worktree descendant remover was deleted.
  `revert` now uses the tree-driven tracked-removal interface when it can prove
  the target state contains a subtree for the path, and otherwise refuses the
  unexpected file-vs-directory mismatch instead of recursing by current ignore
  rules.
- Git command projection now lives under `git_projection` internals. `commit`
  remains the porcelain verb; the broad `switch` surface was retired.
- `maintenance index` and `maintenance monitor` graduated from hidden clap
  subcommands to discoverable admin maintenance commands. Their machine
  contracts were already cataloged under the admin surface.

Lane 6 registered the first deletion-prep migrations in
`crates/repo/src/migration.rs`:

- `0002_canonicalize_thread_records`
- `0003_canonicalize_tree_entries`

These migrations are durable gates. The current deletion pass moved the
remaining old readers behind those gates rather than leaving them mixed into
normal runtime code.

## Compatibility and Refusal Boundaries

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

### Old packed-oplog schemas

Normal packed-oplog loads accept only the V4 container with StateId-native
OpRecord schema 4. Old V2/V3 containers and record schemas 1–3 contain
16-byte ChangeIds that cannot be converted into content-derived StateIds from
the oplog alone. They are therefore refused before entry decoding and are
never rewritten.

Refusal verification:
- `cargo test -p heddle-oplog packed_oplog -- --nocapture`

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
