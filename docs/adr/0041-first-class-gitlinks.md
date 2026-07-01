# First-class Gitlink tree entries

Git submodule pointers are source tree entries, not ordinary file bytes. Heddle
has historically preserved Git gitlinks through an in-band blob convention:
`heddle-submodule:<oid>` stored as a normal file blob on import, then sniffed on
Git export and converted back to a `160000` tree entry. That bridge preserved
round trips, but it made one ordinary file content string semantically special
and forced import, export, status, diff, and materialization paths to remember a
magic blob convention.

This ADR records the replacement model for deleting that bridge.

**Status:** proposed

## Context

Current live readers and writers are:

- `crates/ingest/src/importer.rs` writes synthetic blobs for Git tree entries
  with mode `160000`.
- `crates/cli/src/cli/commands/git_adapter.rs` writes the same synthetic blob
  representation for staged Git-index gitlinks.
- `crates/cli/src/bridge/git_export.rs` sniffs ordinary normal-file blobs whose
  content starts with `heddle-submodule:` and emits Git commit tree entries.
- `crates/objects/src/object/tree.rs` exposes a flat `TreeEntry { name, mode,
  entry_type, hash }`, which permits invalid mode/type combinations and has no
  representation for a foreign Git object target.
- `docs/spikes/heddle-451-schema-versioning-policy.md` row 24 identifies the
  magic-blob collision as an unstamped durable-format hole.

The desired end state is not a better magic prefix. It is a source tree model
where a Gitlink is a typed tree entry target.

## Decision

Add first-class Gitlinks to Heddle trees by moving `TreeEntry` to a closed,
constructor-driven target model:

```rust
pub struct TreeEntry {
    name: String,
    target: TreeEntryTarget,
}

pub enum TreeEntryTarget {
    Blob { hash: ContentHash, executable: bool },
    Tree { hash: ContentHash },
    Symlink { hash: ContentHash },
    Gitlink { target: sley::ObjectId },
}
```

The exact Rust module layout may vary, but these semantics are fixed:

- A Gitlink target is a typed Sley Git object ID. It carries object format plus
  raw object ID bytes; it does not assume SHA-1.
- Sley owns Git object identity semantics and Git operation behavior. Heddle
  owns the stable durable encoding and hashing contract for Heddle source
  history objects.
- The durable tree format becomes an explicit V2 encoding with stable entry
  target tags. It must not rely on Rust enum layout or serde shape as the public
  format.
- Tree identity is computed from semantic entry targets, not from serialized
  bytes. A V2 Gitlink hashes as a stable Gitlink tag plus `{format, raw_oid}`
  plus the entry name.
- Gitlink targets are foreign Git references embedded in tree entries. Native
  Heddle object traversal, pack building, wire transfer, and fsck include the
  tree entry, but they do not fetch or require the target commit as a Heddle
  object.
- Git export may write a `160000` tree entry without requiring the target commit
  object to exist in the superproject object database, matching Git's submodule
  model.

## Creation surfaces

Only Git-aware paths create Gitlinks:

- Git import/ingest from a Git tree entry with mode `160000`.
- Git index adapter paths from a staged Git index entry with mode `160000`.
- The registered migration, and only with Sley proof that the corresponding
  original Git tree entry was mode `160000` with the same target OID.

Ordinary native Heddle snapshot does not infer Gitlinks from `.gitmodules`,
nested `.git` directories, or blob bytes that happen to start with
`heddle-submodule:`.

## Materialization and capture

Native Heddle worktrees materialize Gitlinks as placeholder files by default.
The placeholder is presentation, not source history content:

- An unchanged placeholder preserves the Gitlink during capture.
- Edited placeholder bytes become an ordinary file replacement.
- Deleting the placeholder deletes the Gitlink.
- Replacing the placeholder with a directory creates a normal tree replacement.

Git-overlay checkout/index paths handle true Git index and tree gitlinks through
Sley/Git semantics instead of placeholder inference.

## Diff, status, and output

Diff and status treat Gitlinks as first-class entries:

- Gitlink to Gitlink with a different target is a target change.
- Gitlink versus blob/tree/symlink is a type change.
- Machine output uses structured fields such as `kind: "gitlink"`,
  `old_target`, and `new_target`, with target object format available where the
  output schema carries object IDs.
- Patch export should use Git-compatible submodule diff/index lines where
  possible, not placeholder-file patches.

## Migration

Use a hard, registered migration gate, tentatively
`0006_gitlink_tree_entries`.

The migration decodes the old unversioned tree shape only inside migration code.
Normal runtime writes and accepts the V2 tree shape after migration; it does not
keep a long-lived dual reader for the old magic-blob convention.

Legacy magic blobs are converted conservatively:

- Convert `heddle-submodule:<oid>` blobs to Gitlinks only when Sley can read the
  original mapped Git tree and confirm that the same path was mode `160000` with
  the same OID.
- Ambiguous magic blobs remain ordinary files.
- `verify`, `fsck`, or migration diagnostics should report
  `ambiguous_legacy_gitlink_blob` for those ambiguous blobs.
- A future explicit repair command may convert an ambiguous ordinary file into a
  Gitlink, but runtime/export must not silently reinterpret it.

Converted Gitlinks change semantic tree identity, so affected tree and state
hashes are recomputed. The migration preserves logical `ChangeId`, updates state
body/signature, re-signs locally owned signatures through the existing
`resign_if_owned` path, and refuses foreign signed states that require a rewrite
with actionable advice.

The repo-level format gate must bump with this migration so older binaries
refuse migrated repositories cleanly instead of misreading V2 trees.

## Consequences

This deletes the `heddle-submodule:` magic-blob runtime contract instead of
making it more elaborate. The price is a real tree-format migration and updates
across import, export, materialization, diff, status, fsck, transfer traversal,
and tests.

The payoff is that Heddle's source model can express all standard Git tree
entry categories it intends to preserve, without making ordinary file content
carry hidden meaning.

## Verification floor

- Golden V1 tree fixture and V2 tree fixture.
- Migration test for a proven legacy gitlink blob.
- Migration test for an ambiguous magic-prefix ordinary file.
- Git bridge round-trip tests for SHA-1 and SHA-256-format Gitlink targets where
  Sley support is available.
- Export test proving a Gitlink target object need not exist in the superproject
  object database.
- Diff/status JSON tests for Gitlink target changes and Gitlink/type changes.
- Native worktree materialization/capture tests for unchanged, edited, deleted,
  and directory-replaced placeholders.
- Object traversal/pack/wire tests proving Gitlink targets are not requested as
  Heddle objects.
