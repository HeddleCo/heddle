# sley-notes requirements for Heddle bridge integration

**Audience:** agent implementing changes in the [sley](https://github.com/HeddleCo/sley) repo (`crates/sley-notes`).

**Consumer:** [heddle](https://github.com/HeddleCo/heddle) git bridge — `crates/cli/src/bridge/git_notes.rs`.

**Status:** draft for implementation (2026-06-09).

---

## 1. Goal

Heddle stores bridge metadata in git notes at `refs/notes/heddle`. Each note blob is JSON (`HeddleNote`); the notes tree maps **annotated commit OID → note blob OID**.

We want to **replace Heddle’s hand-rolled notes tree plumbing** with `sley-notes`, while keeping Heddle-specific serde and export/retraction policy in heddle.

Split of responsibility:

| Layer | Owner | Examples |
|---|---|---|
| Git notes mechanics (tree layout, fanout reads, ref advance, CAS) | **sley-notes** | `read_note_for`, `iter_notes`, incremental upsert/remove |
| Product semantics | **heddle** | `HeddleNote`, export backfill, embargo retraction, import tolerance |

---

## 2. Heddle usage context

### Ref and payload

- Notes ref: `refs/notes/heddle` (fixed; Heddle does **not** use `GIT_NOTES_REF` / `core.notesRef` resolution for writes).
- Payload: JSON blobs; Heddle owns serde. sley should treat note bodies as opaque `&[u8]` / blob OIDs.

### Read paths (today)

- `read_note(commit_oid)` — single lookup during import.
- `read_all_notes()` — full scan during identity recovery (`git_mapping.rs`, `fsck.rs`).

### Write paths (today)

- `write_note(commit_oid, note)` — incremental upsert during export/backfill.
- `remove_notes(commit_oids)` — selective retraction during export when commits become unserved (embargo). **Must no-op** when nothing would change (no new commit, no ref churn).

### Ref update semantics Heddle relies on

Heddle advances `refs/notes/heddle` with compare-and-swap:

- First write: `expected = None` (ref must not exist).
- Subsequent writes: `expected = Some(Direct(prior_notes_head))`.

Notes commits are always fast-forward (single parent = previous notes head). This matters because the bridge push path rejects non-FF ref updates.

Reference implementation: `crates/cli/src/bridge/git_notes.rs` in heddle.

---

## 3. What sley-notes already provides (keep; document)

These APIs are sufficient for Heddle’s **read migration** once wired:

| API | Heddle use |
|---|---|
| `read_note_for` / `read_note` | Fanout-aware single lookup |
| `read_note_bytes` | Blob body fetch + non-blob error |
| `iter_notes` / `list_notes` | Bulk import scan without O(n) re-walk per OID |
| `notes_tree_oid` | Peel ref → root tree (commit or direct tree) |
| `NotesRef::expand` | Qualify ref names |
| `write_notes` | Full replace (too coarse for Heddle export, but fine for tests/tools) |
| `upsert_note` / `remove_note` | In-memory helpers only today |

**Tests we depend on conceptually:** fanout read (`read_note_for_skips_unrelated_fanout_branches`, `fanout_tree_is_readable`), system-git interop (`note_bytes_match_system_git`).

No changes required for read APIs unless gaps in §5 are found during implementation.

---

## 4. What we need added (primary ask)

### 4.1 Incremental repo-level upsert

```rust
pub fn upsert_note_for(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    annotated: &ObjectId,
    blob: ObjectId,
    message: &str,
    identity: &NotesCommitIdentity,
    ref_expected: Option<RefTarget>, // CAS on notes ref head
) -> Result<UpsertNoteOutcome>;
```

**Semantics:**

1. Resolve current notes head via `notes_tree_oid` (fanout-aware **read** of existing entries).
2. If a note for `annotated` already exists with the **same** `blob` OID → **no-op** (`UpsertNoteOutcome::Unchanged`). Do not write objects or move the ref.
3. Otherwise build a new flat notes tree:
   - Load all existing notes via `iter_notes` (or internal equivalent).
   - Apply in-memory `upsert_note`.
   - Write flat sorted tree (same layout as `write_notes` today).
4. Create a new notes commit with parent = prior notes head (if any).
5. Update `notes_ref` with `ref_expected` CAS (same pattern as `write_notes`).

```rust
pub enum UpsertNoteOutcome {
    /// New or updated note; ref advanced.
    Updated { notes_commit: ObjectId },
    /// Annotated object already had this blob; no objects or ref written.
    Unchanged,
}
```

### 4.2 Incremental repo-level remove

```rust
pub fn remove_note_for(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    annotated: &ObjectId,
    message: &str,
    identity: &NotesCommitIdentity,
    ref_expected: Option<RefTarget>,
) -> Result<RemoveNoteOutcome>;

pub fn remove_notes_for(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &NotesRef,
    annotated: &[ObjectId], // or HashSet — caller’s choice
    message: &str,
    identity: &NotesCommitIdentity,
    ref_expected: Option<RefTarget>,
) -> Result<RemoveNoteOutcome>;
```

**Semantics:**

1. If notes ref absent → `RemoveNoteOutcome::Unchanged`.
2. If none of the requested annotated OIDs have entries → `Unchanged` (no empty-delta commit).
3. Otherwise remove entries, write new flat tree + notes commit + ref update.
4. `ref_expected` must be `Some(Direct(prior))` when ref exists; `None` only when creating from empty (should not happen on remove — return `Unchanged` if ref missing).

```rust
pub enum RemoveNoteOutcome {
    Removed { notes_commit: ObjectId },
    Unchanged,
}
```

Prefer `remove_notes_for` as the batch API (one commit for N removals). Single-OID wrapper can delegate to it.

### 4.3 Convenience: upsert/remove with body bytes

Optional thin wrappers (nice for CLI; heddle may write blob separately):

```rust
pub fn upsert_note_bytes_for(...) -> Result<UpsertNoteOutcome>;
// writes blob if needed, then calls upsert_note_for
```

Heddle will likely keep blob write in `git-substrate` and call `upsert_note_for` with a precomputed blob OID.

### 4.4 Ref CAS parameter

`write_notes` today sets `expected` from the prior direct OID only. For Heddle:

- Expose `ref_expected: Option<RefTarget>` on **all** mutating entry points (`write_notes`, `upsert_note_for`, `remove_notes_for`).
- `None` means “ref must not exist” (create-only), matching `sley-refs` transaction semantics.
- `Some(Direct(oid))` means “ref must point here or fail”.

Do **not** silently widen to `Any` on mismatch — Heddle needs loud failure for concurrent export safety.

### 4.5 Flat write, fanout read (unchanged policy)

- **Reads:** any fanout depth (already implemented).
- **Writes:** flat sorted tree of full 40-char hex names (already `write_notes` behavior). Git reads this identically; no need to implement fanout writes.

---

## 5. Read-path gaps to verify (minor)

If not already true, ensure:

1. `iter_notes` skips non-hex intermediate names without failing the whole iteration (Heddle mirrors tolerate stray `git notes` entries).
2. `read_note_bytes` returns `Ok(None)` when absent; `Err` only for corrupt/non-blob objects.
3. Symbolic `refs/notes/heddle` resolves correctly (unlikely for Heddle, but import from foreign repos may use symrefs).

No JSON validation in sley — Heddle filters invalid JSON after `read_note_bytes`.

---

## 6. Tests to add in sley-notes

| Test | Asserts |
|---|---|
| `upsert_note_for_unchanged_is_noop` | Same annotated + same blob → `Unchanged`, ref OID unchanged |
| `upsert_note_for_updates_blob` | Same annotated, different blob → new commit, ref moves |
| `upsert_note_for_creates_ref` | Empty repo → first note creates ref with `ref_expected: None` |
| `upsert_note_for_cas_mismatch_fails` | Wrong `ref_expected` → transaction error, ref unchanged |
| `remove_notes_for_partial_hit` | Remove 1 of 2 notes → one commit, survivor remains |
| `remove_notes_for_noop_when_missing` | Ref absent or OIDs not present → `Unchanged` |
| `remove_notes_for_batch_single_commit` | Remove N notes → one new commit, not N |
| `incremental_ops_read_fanout_legacy` | Build fanout tree manually (existing test helper), upsert/remove via new APIs, confirm `read_note_for` still works |
| `incremental_ops_ff_chain` | Two upserts → second commit’s parent is first notes head |

Reuse existing fanout tree builders from `read_note_for_skips_unrelated_fanout_branches`.

---

## 7. Non-goals (do not implement in sley-notes)

- `HeddleNote` schema, JSON, or status/embargo fields.
- `GIT_NOTES_REF` override behavior for Heddle’s fixed `refs/notes/heddle` writes (resolution API can stay for CLI).
- Fanout **write** layout.
- Merge/rebase of notes histories beyond single-parent FF chain.
- Reflog message conventions (“heddle: …”) — caller supplies `message` + `identity`.

---

## 8. Acceptance criteria (heddle-side integration)

After sley ships the above, heddle will:

1. Add workspace dep `sley-notes = { path = "../sley/crates/sley-notes" }`.
2. Change `read_note_repo` / `read_all_notes_repo` to call `read_note_bytes` / `iter_notes`.
3. Change `write_note_repo` to: write JSON blob → `upsert_note_bytes_for` (or blob + `upsert_note_for`).
4. Change `remove_notes_repo` to `remove_notes_for` with batch OID set.
5. Keep `HeddleNote`, export/retraction orchestration, and bridge tests unchanged.

**Green gates:**

- `cargo test -p heddle-cli bridge::git` (110+ bridge tests, including embargo note retraction).
- `cargo test -p sley-notes` (existing + new tests).

---

## 9. Suggested implementation order

1. Add `ref_expected` to `write_notes` (small API extension; keeps one CAS code path).
2. Implement internal helper: `load_all_notes_flat_or_fanout` → `Vec<Note>` (wrap `iter_notes`).
3. Implement `upsert_note_for` + `remove_notes_for` sharing tree/commit/ref logic with `write_notes`.
4. Add tests in §6.
5. (Optional) `upsert_note_bytes_for` wrapper.

---

## 10. Reference links

| Item | Path |
|---|---|
| Heddle notes module | `crates/cli/src/bridge/git_notes.rs` |
| Export write + retract | `crates/cli/src/bridge/git_export.rs` |
| Import read | `crates/cli/src/bridge/git_import.rs`, `git_mapping.rs` |
| Retraction tests | `crates/cli/src/bridge/git_bridge_tests.rs` (`export_retracts_note_for_retracted_commit`, `scoped_export_retracts_note_for_commit_with_embargoed_ancestor`) |
| sley-notes today | `crates/sley-notes/src/lib.rs` |
| Heddle ref CAS | `crates/git-substrate/src/refs.rs` (`RefConstraint`) |

---

## 11. Open question for sley implementer

**Empty notes ref after removing last entry:** should the ref point at a commit on the empty tree (current `write_notes` behavior with `notes: &[]`), or should the ref be deleted?

Heddle today keeps the ref and writes a commit with an empty tree when removing entries (ref still exists). Prefer **keep ref on empty-tree commit** for consistency with existing mirror pushes. If the last note is removed, still emit one FF commit unless the remove is a no-op (nothing to remove).