# heddle#451 — On-disk schema-versioning policy + per-format audit

**Status:** spike deliverable (decision doc). **Motivated by:** the #449 brick — the
oplog `OpRecord` payload schema changed with no version discriminator; the new reader
silently misparsed legacy bytes and bricked pre-cutover repos. **Scope:** every on-disk
format a v0.3.0 binary writes, audited against the discipline below; the bump/migration
rules every post-0.3 format change must follow; the test floor; the pre-1.0 exception
ledger. All file:line citations are at origin/main `86bd10e8` (post-#352 collapse).

This doc is the linking target for the fidelity-epic issues (#564/#593/#575/#606),
the packed-oplog work (#618), and any future format change. A format-touching PR that
cannot point at the row of §2 it updates and the rule of §3 it follows is not mergeable.

---

## 1. Verdict

**v0.3.0 may ship after one XS fast-follow lands: enforce `[repository] version` at
`Repository::open`.** Today that field is written at init
(`crates/repo/src/repo_config.rs:429`, struct at `repo_config.rs:240-242`) and **read by
nothing** — a dead stamp. Every *container* format is individually versioned and refuses
newer versions, so nothing silently corrupts today; but the object-store record payloads
(State/Tree/Action) carry **no version discriminator**, and the planned #593 change
would otherwise force either probe-decoding (the #449 anti-pattern) or bricking.
Enforcing the repo-level version turns every future bump into a clean
refuse-with-advice for older binaries and a ledgered migration for newer ones.

## 2. Version-stamp inventory

“Old↦new” = what TODAY’s binary does when it meets bytes written by a hypothetical
newer format revision of that surface. “New↦old” = what a future binary must be able
to do with today’s bytes.

| # | Surface (path under `.heddle/` unless noted) | Codec | Version stamp | Old↦new behavior today | Evidence |
|---|---|---|---|---|---|
| 1 | **oplog container** `oplog.bin` | binary | `LMOPLOG\0` magic + `version: u32`; v4 = latest, v2/v3 decode-only migration sources | refuse: `unsupported oplog version {n}` from `load()` dispatch; eager atomic (temp+rename) migration of v2/v3/stale-v4 on open via `ensure_latest` | `crates/oplog/src/oplog/packed_oplog.rs:28` (magic), `:59-62` (sealed `V2/V3/V4`, `Latest = V4`), `:1018-1028` (`load()` refusal), `:247-276` (`ensure_latest` migrate + `write_file_atomic`), `:284-302` (`read_head_id` refuses non-latest) |
| 2 | **OpRecord payload schema** (entries inside #1) | rmp-serde, positional | `record_schema_version: u32` in the v4 header; sealed decode-only schema types v1 (`pre-atomic-v1`), v2 (`atomic-no-head-v2`), v3 (`current-v3`) | refuse: `unsupported OpRecord schema version {n}` (`schema_version_from_u32`); reader selects decode path by stored version, never probes — except the legacy `parse_unversioned_entry` path reachable only from v2/v3 containers | `crates/oplog/src/oplog/op_record_codec.rs:9-18` (invariant), `:20-25` (#352 exception, §6), `packed_oplog.rs:122-157` (v4 decode selects by header schema), `:1513-1527` (probe path, v2/v3 only) |
| 3 | **oplog EOF index footer** | binary | `LMOPIDX\0` + `INDEX_VERSION = 1` | refuse (`unsupported oplog index version`) | `packed_oplog.rs:29-30,905` |
| 4 | **packfiles** `objects/packs/*.pack` | binary | `LMPK` magic + `version = 2` + blake3 trailer | refuse: `Unsupported pack version` / `Pack checksum mismatch`; unknown entry `ObjectType`/id-tag byte also refuses | `crates/objects/src/store/pack/mod.rs:41-46` (spec), `shared.rs:100-129` (`verify_container`), `mod.rs:48-58` (`ObjectType::from_u8` → `None` → error), `shared.rs:62-64` (unknown id tag) |
| 5 | **pack index** `*.idx` | binary | `LMI\0` + `INDEX_VERSION = 2` | refuse | `pack_index.rs:6-7,69-78` |
| 6 | **loose-object compression envelope** (all of blobs/trees/states/actions) | 1 tag byte + u64 size + payload | tag byte is `CompressionType` 0/1/2, not a format version; `is_compressed` *sniffs* (zstd magic check), short inputs “assume uncompressed” | unknown tag → `InvalidType` error; the sniffing is in-band heuristic, not a stamp | `crates/objects/src/store/compression/mod.rs:14-40,238-262,289-298` |
| 7 | **State objects** `objects/states/**` (also embedded in packs) | rmp-serde, **positional** | **NONE.** Implicit convention only: tail-append optional fields with `#[serde(default)]`, documented on the struct | **accidental refuse**: rmp positional decode of a longer tuple fails `LengthMismatch` (verified empirically — even when the new tail fields are `None`, because rmp serializes `None` as nil, not absent); a same-arity *type* change fails with a type error both directions. Never silent-misparse for struct changes, but the error is a raw serialization error with zero advice, and nothing selects a decode path | `crates/objects/src/object/state_core.rs:187-200` (tail invariant doc), `:215` (tail marker); writer `crates/objects/src/store/fs/fs_impl.rs:719` (`rmp_serde::to_vec(state)`); reader validates only the embedded id, not bytes: `fs_impl.rs:70-79` |
| 8 | **Tree objects** `objects/trees/**` | rmp-serde, positional | **NONE.** `TreeEntry` = 4 required fields | same accidental-refuse class as #7. NOTE: tree/state IDs are **custom hash framings, not hashes of the rmp bytes** (`ContentHash::compute_typed_with_len("tree", …)`), so re-encoding bytes is ID-stable — store migrations are possible without rewriting history | `crates/objects/src/object/tree.rs:122-127` (entry), `:270-277` (framing hash); `fs_impl.rs:56-68` (load recomputes framing hash, catches corruption) |
| 9 | **Action objects** `objects/actions/**` | rmp-serde, positional | **NONE** | same class; load recomputes `compute_id` | `fs_impl.rs:86-101` |
| 10 | **Versioned sidecar blobs** (provenance, risk signals, review signatures, discussions, structured conflict, visibility sidecars) — content-addressed, referenced by hash from State tail fields | rmp-serde | `format_version: u8` per blob via `versioned_msgpack_blob!`; provenance hand-rolls the same | refuse: `unsupported … version {n}` (strict `!=` for most; risk-signal/op-index use `>` reject-newer) | `crates/objects/src/object/versioned_blob.rs:14-19,50-51` (macro), `state_provenance.rs:64,84-86`, `structured_conflict.rs:28-34,158-160`, `risk_signal.rs:28-42` |
| 11 | **HEAD** + loose refs `refs/threads/*`, `refs/markers/*`, remotes | bare text (`ref: <thread>` / hex ChangeId) | NONE | parse-error refuse (`HeadParseError` / `ChangeIdTextError`) — no silent misread, no advice | `crates/refs/src/refs/head.rs:19-30`, `text.rs:10-13` |
| 12 | **packed-refs** | git-style text | NONE (comment header only) | ⚠️ **silently skips** unparseable/unknown lines (`continue`) — a future line-format extension would silently vanish refs from an old binary’s view. The one surface today whose failure mode is silent-drop rather than refuse | `crates/refs/src/refs/packed_model.rs:24-46` |
| 13 | **ref summary index** (cache) | text | `heddle-ref-summary-v1` first line, strict match | refuse → callers fall back to enumerating storage; rebuilt on write | `crates/refs/src/refs/ref_summary_index.rs:14,107-110`; fallback `refs_manager.rs:621-625` |
| 14 | **operation log index** `cache/operation_index/buckets/*` (cache) | rmp-serde | `format_version: u8 = 1` per bucket; rejects **newer only** | refuse newer; cache — rebuildable | `crates/refs/src/refs/operation_index.rs:38,151-157,279,297` |
| 15 | **worktree index** `state/index.bin` + journal (cache) | binary | `HDLEIDX\0` + `INDEX_VERSION = 5` (multi-version reader v1–v5); journal `HDLEJNL\0` v1 | refuse `VersionMismatch` → caller logs + proceeds with a fresh empty index (rescan) — correct cache semantics | `crates/repo/src/worktree_index.rs:69-83`, `worktree_index_storage.rs:105-122`; fallback `repository_tree.rs:230-236` |
| 16 | **commit graph** `LMGRAPH\0` (cache) | binary | `GRAPH_VERSION = 1` | refuse → warn + rebuild empty | `crates/repo/src/commit_graph_persistence.rs:17-18,128-130`; `commit_graph.rs:50-78` |
| 17 | **thread records** `thread_records/*.json` + `Thread` JSON | serde_json (named) | NONE — but self-describing; `#[serde(default)]` throughout, unknown fields ignored | additive changes tolerated both directions; renames/type changes refuse | `crates/repo/src/thread_storage.rs:24-60`, `thread_record_store.rs` |
| 18 | **thread manifest** `manifest.toml` | TOML | `SCHEMA_VERSION: u32 = 3`, strict `!=` refuse | refuse, with the **model error message**: “manifest at … uses schema {x} but this binary speaks {y}” | `crates/repo/src/thread_manifest.rs:48,346-356` |
| 19 | **repo config** `config.toml` | TOML | `[repository] version = 1` — **written, never read** | ⚠️ blind-proceed: an old binary opening a hypothetical version-2 repo checks nothing | `crates/repo/src/repo_config.rs:240-242` (field), `:429` (default write); zero read sites (grep `repository.version`) |
| 20 | **migration ledger** `state/schema_versions.toml` | TOML set of applied migration ids | self-describing | declarative forward-only migration framework, idempotent, atomic save; 1 registered migration | `crates/repo/src/migration.rs:1-30,119-153,163-168` |
| 21 | **JSON state sidecars**: stash, sessions, merge state, `REBASE_STATE`/`BISECT_*` etc. | serde_json (named) | NONE | additive-tolerant; ⚠️ list paths skip unreadable entries silently (`let Ok(..) else continue`) | `crates/repo/src/stash.rs:67,91`, `session_storage.rs:125,150`, `merge_state.rs:190,213` |
| 22 | **identity** `identity.toml` / `device-identity.toml`; agents `agents/*.toml`; `fsmonitor.toml`; `lazy-hydrator.toml`; `shallow` (text id list); worktree `objectstore` pointer (text path) | TOML/text | NONE | tolerant / parse-error refuse; agents are ephemeral runtime state | `identity.rs:188-319`, `agent_registry.rs:4`, `shallow.rs:22-25` |
| 23 | **git-bridge mirror** + `git-bridge/bridge-mapping.json` | real git repo + JSON | NONE on the mapping | additive-tolerant JSON sidecar plus `refs/notes/heddle` rebuild path; the mirror itself is git-format (sley/git-owned). Legacy `.heddle/git/bridge-mapping.json` migration and `Heddle-Change-Id` trailer rebuild paths are removed. Mirror is slated for deletion (#568) | `crates/cli/src/bridge/git_mapping.rs:20-37,70-122`; `crates/cli/src/bridge/git_notes.rs` |
| 24 | **legacy `heddle-submodule:` gitlink blob convention** | in-band magic prefix inside ordinary blob *content* | NONE | live import/export bridge convention — import paths still synthesize ordinary blobs whose bytes are `heddle-submodule:<oid>`, and export sniffs any `FileMode::Normal` blob with that content and silently emits a gitlink. A user file with exactly that content is silently misinterpreted (low-probability, real class). Deletion needs a first-class gitlink tree entry or metadata representation plus a migration/escape policy for existing magic-prefix blobs. | `importer.rs`, `git_adapter.rs`, `git_export.rs` |
| 25 | **reftable** `REFT01\0\0` | binary | magic, no version int | **dead spike — no production writer**; if ever revived it enters this policy with a migration from refs-text and a real version int | `crates/refs/src/refs/reftable_model.rs:41` |

### Empirical rmp-serde ground truth (drives rules R4/R5)

Verified 2026-06-12 against rmp-serde 1.x (the workspace’s codec):

| Scenario | Result |
|---|---|
| old struct decodes bytes with extra `#[serde(default)]` tail fields | **`Err(LengthMismatch)`** — even when the tails are `None` (rmp writes nil, not absent) |
| old struct decodes bytes whose tail fields used `skip_serializing_if = "Option::is_none"` and were `None` | **Ok** (array is shorter; byte-identical to pre-change encoding) |
| new struct decodes old bytes (missing `#[serde(default)]` tails) | **Ok** (defaults) |
| `String` field decodes bytes written as `Vec<u8>` (and vice versa) | **Err(type mismatch)** both directions — the #593 hazard |

## 3. The policy

### R1 — Every persistent surface carries an explicit version discriminator
Durable data (anything a user would grieve losing) MUST carry magic + version
(binary) or a schema-version field (TOML/JSON/rmp). The reader **selects a decode path
by version; it never blind-deserializes and hopes, and never probe-decodes** (probing
is what #449 weaponized; the only sanctioned probe is the frozen
`parse_unversioned_entry` path for pre-versioned v2/v3 oplogs,
`packed_oplog.rs:1513-1527`). Rebuildable **caches** (rows 13–16) may instead
refuse-and-rebuild — that is their version policy, and it must be a deliberate,
caller-visible fallback (the `repository_tree.rs:230-236` pattern), not an accident.

### R2 — The repo-level format gate (the new rule this spike adds)
`config.toml [repository] version` is THE coarse gate protecting surfaces that are
impractical to stamp per-record (rows 7–9, 11–12, 21–24). Rules:
- `Repository::open` MUST refuse `version > SUPPORTED_REPO_FORMAT` with advice (R3).
- Any format change that older binaries could otherwise *silently* mishandle — or that
  changes unstamped-surface bytes — MUST bump the repo version as part of its
  migration, so older binaries refuse the whole repo instead of tripping mid-operation.
- Per-surface stamps (oplog, packs, …) still bump independently for changes that are
  locally detected and locally migratable; the repo version is the floor, not a
  replacement.

### R3 — What an older binary MUST do on encountering newer: refuse with advice, never crash/corrupt
The mandatory outcome is a **clean, actionable refusal**: name the surface, the found
vs supported version, and the way out (“upgrade heddle” / “run `heddle migrate`”).
Today’s refusals are mostly raw `InvalidObject("unsupported oplog version 5")` — safe
but unhelpful. New code uses a dedicated error (e.g. `HeddleError::FormatTooNew
{ surface, found, supported }`) rendered with that advice; existing sites migrate
opportunistically. The `thread_manifest.rs:346-356` message (“uses schema {x} but this
binary speaks {y}”) is the floor for wording. **Forbidden outcomes:** silent misparse,
silent drop (the packed-refs `continue` — row 12), panic, or partial writes against a
half-understood file.

### R4 — Schema-evolution rules per codec family
- **Positional rmp structs (State/Tree/Action, OpRecord):** tail-append only, with
  `#[serde(default)]` AND `#[serde(skip_serializing_if = "Option::is_none")]` so that
  records not using the new field stay **byte-identical** (older binaries keep reading
  them; only records that actually carry the new data refuse). Mid-struct inserts,
  reorders, and in-place type changes (`String`→`Vec<u8>`) are **version bumps**, full
  stop. Plain-`default` tails (no skip) are also a bump: they break old readers on
  every record (see §2 ground truth). The existing always-serialized tails on `State`
  predate this rule; they are grandfathered, not precedent.
- **rmp enums (OpRecord):** any variant reshape/removal/reorder is a bump — add a new
  sealed frozen schema per `op_record_codec.rs:9-18`. Appending a variant is additive
  (older binaries refuse only records using it) but must still be ledgered (§5 fixture).
- **Named JSON/TOML:** additive optional fields are free; renames/type
  changes/semantic re-interpretations are a bump of that file’s `schema_version` (add
  one if absent — the thread-manifest pattern).
- **Binary containers:** any layout change bumps the container version; the new reader
  keeps a decode path (or migration) for every prior version ever shipped in a public
  binary.
- **Hash framings (State/Tree id computation):** changes are the most expensive kind —
  they re-identify history. Post-0.3 they require an epic-level migration plan
  (the #564 backfill is the template), never ride along with a field add. Sparse
  framing additions follow the `authored_at` pattern: absence is encoded so that
  records without the feature hash identically to before (`state_core.rs:234-241`).

### R5 — Migration shapes: forward-only, ledgered, atomic, idempotent
- All migrations are **forward-only** (no downgrade rewriters; downgrade = R3 refusal)
  and registered in `migration.rs::MIGRATIONS`, recorded in
  `state/schema_versions.toml`. Idempotent; atomic temp+rename writes only.
- **Eager rewrite-on-open** is the default for bounded files (the
  `PackedOpLog::ensure_latest` model, `packed_oplog.rs:247-276`). **Lazy/rewrite-on-read**
  is permitted only for content-addressed object payloads where eager rewriting the
  whole store is unbounded — but #593-class type changes should still migrate eagerly
  (states are small; the rewrite is ID-stable per row 8 note).
- A migration that changes what older binaries would see MUST bump
  `[repository] version` in the same ledgered step (R2).
- Old decode paths for versions shipped in a public binary are kept ≥ one minor
  release after their migration lands; pre-public versions may be dropped via §6.

### R6 — Close the silent-drop holes
packed-refs (row 12) and the silent-skip list loaders (row 21) must either warn loudly
or refuse on unparseable entries. A versioned header line for packed-refs rides the
next refs-format touch.

## 4. Planned post-0.3 changes mapped against the inventory

| Change | Surfaces hit | Under current stamping | Required treatment |
|---|---|---|---|
| **#593** Principal `String`→`Vec<u8>` (fidelity epic #564/#568) | rows 7, 19 | **Would brick**: new binary cannot decode any existing State (type error, §2 ground truth); no version exists to select a legacy decode path — naive impl forces probe-decoding (#449 anti-pattern) | Bump: repo version → 2 with an eager, ledgered store migration rewriting all states (ID-stable). 0.3 binaries then refuse migrated repos cleanly **iff the R2 gate shipped in 0.3** |
| **#575** first-class annotated-tag objects | rows 4, 7, 11 | New pack `ObjectType` → 0.3 refuses *any* pack containing a tag entry (opaque error mid-sync); loose layout addition is invisible/harmless; marker→tag-object reference is a new ref payload shape | Pack version bump (or tag-bearing packs only) + repo-version bump; sync/hosted transfer must negotiate or refuse old clients |
| **#606** verbatim nonstandard tree modes | row 8 + tree hash framing | Safe **only** as a trailing `skip_serializing_if` sparse field (standard trees stay byte-identical; only nonstandard-mode trees refuse on 0.3) + absence-preserving hash framing (R4). A plain-default tail would make every new tree unreadable by 0.3 | Additive under R4; conformance fixtures (§5); ledger entry; no repo-version bump needed if sparse |
| **#618** oplog truncation recovery | rows 1, 3 (+ new `.oplog.recovery` sidecar) | No format change; pure robustness win (today a <120-byte tail truncation makes the whole oplog unreadable — `packed_oplog.rs:136`, `FOOTER_LEN:34`) | Additive; recovery sidecar gets a version field at birth (R1) |
| **#265** submodule status (half-recurse) | row 24 (read-only) | Display-only; no format change | None. The in-band-prefix collision class stays open (follow-up F5) |

## 5. Test floor — cross-version conformance per stamped surface

Each stamped surface keeps, in-tree:
1. **A checked-in golden fixture per legacy version** ever shipped (or, pre-public,
   per version still decodable), with a round-trip test: current binary decodes the
   fixture and re-encodes to the latest version losslessly. The op-record fixtures
   (`op_record_codec.rs` tests + `tests_support` frozen encoders) are the template.
2. **An old-reads-new refusal test**: the *frozen previous* decoder must refuse (not
   misparse) bytes encoded by the current schema — exactly
   `migration_sensitive_legacy_shapes_do_not_decode_as_current`
   (`op_record_codec.rs`) generalized. For unstamped-but-grandfathered surfaces
   (State/Tree), this is the LengthMismatch/type-error assertion.
3. **A golden-bytes format-lock test (the CI guard, recommendation for the issue’s
   stretch goal):** serialize one canonical exemplar of each surface and byte-compare
   against a checked-in fixture. Any serialization change fails CI until the author
   (a) bumps the surface’s version (or files a §6 exception), (b) adds the new golden
   fixture *alongside* the old one, and (c) registers the migration. This catches the
   #449 class at PR time with zero runtime machinery — no proc-macro/schema-diff
   tooling needed. Surfaces: OpRecord (exists in spirit), oplog container header,
   State, Tree, Action, pack entry, each `versioned_msgpack_blob!` type, thread
   manifest, worktree-index header.

## 6. Pre-1.0 exception ledger

Pre-1.0, the no-backcompat stance sometimes justifies skipping a bump/migration. An
exception is legitimate ONLY if: (a) no public binary ever wrote the old bytes, (b)
the exception **documents itself in the codec header** at the change site, and (c) it
is recorded here. The #352 entry is the template:

| Date | Change | Exception taken | Self-documentation | Why safe |
|---|---|---|---|---|
| 2026-06-12 | #352 V2 OpRecord variant collapse (`ThreadCreateV2`→`ThreadCreate`, `FastForwardV2`→`FastForward`), commit `86bd10e8` | Rewrote the frozen v1/v2 schema mirrors to the collapsed shapes instead of adding a v4 record schema; old dev oplogs containing those records no longer decode | `op_record_codec.rs:20-25`: “Documented exception (#352, pre-v0.3.0)… This exception must not be repeated once public binaries exist.” | No production oplogs exist pre-0.3; dev logs discardable |

**This lane closes the day v0.3.0 binaries are public.** From then on every format
change follows §3 in full.

## 7. Recommended follow-up issues (for the orchestrator to file on approval)

- **F1 (pre-0.3 fast-follow, XS, blocks confident 0.3):** enforce
  `config.repository.version` at `Repository::open` — refuse newer with R3 advice;
  test: handcrafted `version = 99` config refuses with the advice string.
- **F2 (S):** `HeddleError::FormatTooNew` + migrate the refusal sites
  (rows 1–5, 13–16, 18) to it.
- **F3 (M):** golden-bytes format-lock CI guard per §5.3.
- **F4 (S):** packed-refs + JSON-list silent-skip → warn/refuse (R6).
- **F5 (S, ride-along with #575/#566):** import-side guard for the
  `heddle-submodule:` content collision (refuse or escape ordinary blobs that match
  the prefix on export).
- **#618** (already filed) gains a reference to R3/R5 and §4’s row.
