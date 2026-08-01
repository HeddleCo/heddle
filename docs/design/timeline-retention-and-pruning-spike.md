# Timeline retention and pruning for immutable branching state

**Status:** Spike complete; proposal only. No behavior change is included.

**Baseline:** `84e4a3a52d3aa859cd3a4c4304921cee85402df0` (`origin/main` and
`HEAD` matched before the investigation).

**Recommendation:** Do not automatically prune canonical timeline operations,
oplog entries, or source states in v1. Put a finite, user-visible creation budget
around every agent run and hosted repository, reject or pause before accepting
bytes beyond that budget, and reclaim only rebuildable views/indexes and
explicitly time-bounded operational caches.

If reclamation of canonical history later becomes necessary, add explicit
timeline retention roots and reduce unprotected spent branches to typed,
signed retention tombstones; never make a persisted cursor resolve to a missing
operation or missing state.

## Scope and source provenance

This report was read and measured from the baseline above. The repository's
documentation convention puts evolving spikes and design notes in
`docs/design/`, reserving `docs/adr/` for durable decisions
(`docs/adr/README.md:3-11`). Every source citation below is relative to that
checkout; no sibling checkout supplied source facts.

Measurements were taken on 2026-08-01. Real stores were read only. Synthetic
stores were created below `/home/scratch`, each with its own `.git` directory so
repository discovery could not walk up into another checkout. No GC command was
run against a real store, and `--prune` was not run at all.

## Findings in brief

1. **Nothing today prunes timeline branches, canonical timeline operations,
   source states, or oplog entries.** Local GC consolidates representations; it
   does not collect native history.
2. A timeline branch is not a copied tree. In the measured shape, one additional
   `FanOut` branch cost **about 0.8 KiB logical** (798–814 B across adjacent
   measurements), and one additional 4 KiB filesystem block after shard
   directories were warm.
3. A small capture without a client operation ID added two oplog entries totaling
   **556–559 B, or about 0.28 KiB/entry on average**. The exact marginal entry
   size depends on the operation body, actor strings, scope, batch, transaction
   key, and optional operation ID.
4. The quoted `state/` result is real but its interpretation was wrong: in the
   three largest examples, **more than 99% of apparent `state/` bytes were the
   idempotency response cache**, not source states, timeline branches, or agent
   reasoning.
5. Raw transcripts and raw tool payloads are not copied into repository history
   by default. The dangerous case is a runaway agent creating accepted timeline
   operations and especially captured changed content at machine speed, not a
   year of ordinary use.

## What exists today

### Undo, redo, and branching are append-only

The invariant in the brief is present in the model:

- `Undo` and `Redo` are reasons for a `CursorMoved` operation
  (`crates/object-model/src/object/timeline.rs:275-284`).
- `EditFromRewoundCursor`, `Retry`, and `FanOut` are explicit branch reasons
  (`crates/object-model/src/object/timeline.rs:286-294`).
- A cursor move records old/new step and state IDs; a branch creation records its
  parent branch, fork step, and fork state
  (`crates/object-model/src/object/timeline.rs:420-443`).
- Navigation rebuilds all branches and all steps, then derives undo/redo targets
  without deleting either (`crates/repo/src/timeline_navigation.rs:106-201`,
  `crates/repo/src/timeline_view.rs:386-428`). Applying a branch operation adds
  or updates the branch and moves the cursor; it does not remove its parent
  (`crates/repo/src/timeline_view.rs:530-552`).

Canonical timeline operations are content-addressed from canonical bytes
(`crates/object-model/src/object/timeline.rs:74-83`,
`crates/object-model/src/object/timeline.rs:296-331`). The local store writes one
sharded immutable file and appends its ID to a rewritten derived index
(`crates/repo/src/timeline_store.rs:123-152`,
`crates/repo/src/timeline_store.rs:179-184`). The only removal in the timeline
store clears a completed materialization-recovery sidecar, not history
(`crates/repo/src/timeline_store.rs:264-271`).

A code search for `remove`, `truncate`, `prune`, `retain`, and `delete` across
the current timeline navigation, view, action, and store modules found no
canonical-history deletion (`crates/repo/src/timeline_actions.rs:38-84`,
`crates/repo/src/timeline_navigation.rs:106-201`,
`crates/repo/src/timeline_store.rs:123-218`). The view's missing-file
behavior is actually a warning for future GC design: when an indexed operation
file is absent, rebuild silently omits it and repairs the index around the
remaining files (`crates/repo/src/timeline_view.rs:653-705`). Deleting operation
files without a tombstone-aware reader would therefore make history disappear,
not produce a safe or intelligible retention boundary.

There is one nearby feature named “collapse,” but it confirms rather than
weakens the invariant. Expired ephemeral source threads are marked abandoned
and get a new oplog record while their underlying states stay addressable
(`crates/repo/src/ephemeral_thread.rs:1-12`,
`crates/cli/src/cli/commands/ephemeral_sweep.rs:4-13`).

**Qualification:** the current model and explicit `timeline fork` command can
record `EditFromRewoundCursor`, but the OpenCode recording bridge simply
continues on the current branch and current step
(`crates/cli/src/harness/mod.rs:654-688`,
`crates/cli/src/harness/mod.rs:804-818`). No automatic “edit after rewind means
fork” detection was found in this checkout. Whether every external adapter
always calls explicit fork is **UNKNOWN**; an end-to-end adapter audit would
settle it. This does not change the append-only finding.

### Current GC is object representation maintenance, not history GC

`heddle maintenance gc` is local repository maintenance
(`crates/cli/src/cli/commands/gc.rs:60-70`). Its actual behavior is:

- dry-run counts loose blobs and trees only
  (`crates/cli/src/cli/commands/gc.rs:84-96`);
- non-dry-run consolidates native objects and packs refs
  (`crates/cli/src/cli/commands/gc.rs:97-106`);
- it removes Git-projection mapping rows whose Git commits are unreachable
  (`crates/cli/src/cli/commands/gc.rs:108-116`,
  `crates/git-projection/src/git_mapping.rs:218-227`);
- it losslessly packs the Git bridge mirror's complete on-disk object set
  (`crates/git-projection/src/git_mapping.rs:230-272`);
- it removes loose blob/tree copies only after a packed canonical copy exists,
  and removes incomplete or unpaired pack artifacts
  (`crates/cli/src/cli/commands/gc.rs:134-177`,
  `crates/objects/src/store/fs/fs_pack.rs:533-591`); and
- its repack carries forward **every** object already in packs, including states
  and attachments, rather than applying a native reachability filter
  (`crates/objects/src/store/fs/fs_pack.rs:279-398`).

The `prune` boolean is deliberately ignored for the loose-copy step
(`crates/cli/src/cli/commands/gc.rs:134-145`). This conflicts with the CLI help's
“unreachable objects” wording (`crates/cli-args/src/cli/cli_args/commands_main.rs:504-525`)
and with future-looking undo/stability prose
(`docs/undo.md:99-103`, `docs/STABILITY.md:243-250`). The implementation is the
source of truth for this report: there is no native history pruning today.

The redaction check is the correct precedent. GC snapshots every redaction
record, then fails loudly if any file disappears or any redaction count drops
(`crates/cli/src/cli/commands/gc.rs:72-82`,
`crates/cli/src/cli/commands/gc.rs:179-209`). A timeline-aware GC needs an
equally mechanical assertion, described below.

### Hosted retention is not implemented here

Agent timelines are a local foundation. The domain model says hosted projection
is planned (`CONTEXT.md:163-180`), and the architecture is explicit that the
planned `AgentGatewayService` and `AgentService` are not registered in Weft
(`docs/ARCHITECTURE.md:215-226`). The timeline ADR likewise says Weft timeline
ingest and querying are not live (`docs/adr/0039-versioned-agent-timeline-operations.md:5-18`).

Consequently there is no hosted timeline retention implementation to audit in
this checkout. Existing collaboration guidance retains valid operations in v1
and defers retention to explicit policy, likely at namespace level
(`docs/adr/0029-retain-valid-collaboration-operations.md:1-7`). The exact deployed
Weft object, database, backup, and idempotency-retention policies are
**UNKNOWN**; auditing the exact deployed Weft commit and storage configuration
would settle them. This spike should be treated as a pre-launch requirement for
hosted timeline billing, not evidence that hosted timeline billing already
occurs.

## Measurements

### Method

The source baseline was built as `heddle 0.11.0` with:

```text
TMPDIR=/home/scratch
CARGO_HOME=/home/scratch/heddle-gc-spike-cargo-home-84e4a3a
CARGO_TARGET_DIR=/home/scratch/heddle-gc-spike-target-84e4a3a
cargo build -p heddle-cli --bin heddle \
  --no-default-features --features git-overlay,native,local,zstd
```

The reduced feature set excludes hosted transport but includes all local
timeline, object, oplog, Git-overlay, and GC paths measured here. Synthetic
fixtures used fixed-length IDs and summaries. Sizes were recorded both as
apparent bytes (`du -B1 --apparent-size`) and allocated bytes (`du -B1` plus
`stat`). The build completed with four unrelated unused-code warnings; no source
or Rust file was changed.

For real-store orientation, `find /home/heddleco -type f
-path '*/.heddle/config.toml'` found 18 configured stores. The table below shows
the three stores with the largest `state/operation_dedup.bin`; it is a
convenience sample, not a representative workload study.

### Re-measured real stores

Each cell is `allocated KiB / apparent KiB`.

| Store | `state/` | `objects/` | `packs/` | `oplog/` | `ingest/` | Total |
|---|---:|---:|---:|---:|---:|---:|
| A | 128 / 111.6 | 116 / 5.3 | 48 / 11.5 | 12 / 5.7 | — | 372 / 135.1 |
| B | 324 / 298.5 | 116 / 4.4 | 64 / 3.4 | 16 / 9.1 | 68 / 49.0 | 672 / 365.4 |
| C | 116 / 93.5 | 60 / 1.9 | 44 / 8.3 | 8 / 3.3 | 24 / 16.0 | 328 / 123.9 |

The idempotency cache alone was 113,427 B, 303,643 B, and 94,946 B,
respectively: **99.2%, 99.4%, and 99.2% of apparent `state/`**. It stores the full
cached response for each operation ID
(`crates/repo/src/operation_dedup.rs:51-80`). A seven-day default and a compact
method exist (`crates/repo/src/operation_dedup.rs:44-49`,
`crates/repo/src/operation_dedup.rs:459-472`), but a repository-wide search found
no production caller of `compact` or `DEFAULT_RETENTION_SECS`; the only compact
calls are tests in that same file. The module comment says periodic maintenance
runs it (`crates/repo/src/operation_dedup.rs:12-19`), but current wiring does not.

This cache is operational idempotency state, not immutable source or timeline
history. It explains the observed `state/ > objects/` result and is the first
thing storage accounting must classify correctly.

### Marginal branch cost

A fixture first recorded one stable source state and one tool step, then created
1,000 nested branches using `--reason fan-out`. At a 1,000-branch history, the
exact-baseline build produced this marginal result:

| Component | One more branch (three adjacent samples) |
|---|---:|
| Canonical `BranchCreated` operation | 314–316 B |
| Rebuildable operation index | 48–51 B |
| Rebuildable timeline-view checkpoint | 434–449 B |
| **Total apparent local growth** | **798–814 B** |
| **Allocated local growth, warm shards** | **4,096 B** |

Across the first 1,000 branches the fixture measured about 812 B/branch. Three
adjacent exact-baseline measurements at that history were 798 B, 800 B, and 814
B; the small difference comes from compact-encoding width thresholds and ID
contents.

The canonical operation contains IDs and one `from_state`; it does not contain a
tree or file bytes (`crates/object-model/src/object/timeline.rs:433-443`). The
operation index and view checkpoint are derived and replaced atomically, not
retained as one historical copy per append
(`crates/repo/src/timeline_store.rs:201-218`,
`crates/repo/src/timeline_view.rs:827-850`). Thus:

- **Marginal canonical branch truth:** about **0.31 KiB**.
- **Marginal local logical footprint with derived data:** about **0.8 KiB**.
- **Marginal local allocation after shards are warm:** usually **4 KiB**, because
  each loose operation occupies a filesystem block. Early allocation was higher
  while random two-hex-digit shard directories were being created.
- **Hosted marginal:** **UNKNOWN**. There is no hosted timeline store here; it
  will be canonical bytes plus database/object metadata and indexes, not
  necessarily the local derived-view shape. A Weft schema and billing-meter
  benchmark would settle it.

The debug CLI created the first 99 branches in 5.06 s and the next 900 in 73.88
s. The exact-baseline debug build took 156 ms for branch 1,001. This slowdown is
consistent with rewriting the growing operation index and view on every append
(`crates/repo/src/timeline_store.rs:144-150`,
`crates/repo/src/timeline_view.rs:228-258`). It is an implementation bottleneck,
not a safety policy: a daemon, batching writer, or hosted endpoint can remove
it.

### Marginal tool step and oplog operation

A correctly paired tool start/finish at a 100-step history added 2,398–2,405 B
across two adjacent measurements. One sample decomposed as:

| Component | One tool call (two timeline operations) |
|---|---:|
| Canonical operations | 1,227 B |
| Rebuildable operation index | 101 B |
| Rebuildable view checkpoint | 1,077 B |
| **Total apparent local growth** | **2,405 B** |

The stored payload is a scrubbed summary and optional hash, not raw arguments or
output (`crates/object-model/src/object/timeline.rs:238-263`).

A one-line, same-size content capture without `--op-id` added two oplog entries
and grew `oplog.bin` by 556–559 B: **about 278–280 B per entry on average**.
Repeating with an operation ID added three entries totaling 1,003 B (**334
B/entry**) and grew the idempotency response cache by 30,058 B. This does not
mean every oplog record is 0.28 KiB. The packed format has a 32 B header, 120 B
footer, 16 B per entry-offset
record, 48 B per batch-directory record, and 32 B per transaction-directory
record (`crates/oplog/src/oplog/packed_oplog.rs:31-43`). Each encoded entry also
has 44 fixed bytes, then variable scope, operation body, actor name/email, and an
optional 16-byte operation ID (`crates/oplog/src/oplog/packed_oplog.rs:2302-2339`).

The durable retained-byte slope is linear, but append I/O currently is not: the
packed oplog preserves the entry prefix and rebuilds tail indexes/footer, with a
source TODO for segmentation/rollover
(`crates/oplog/src/oplog/packed_oplog.rs:614-744`). There is no oplog retention or
pruning path in current GC.

### Source-state and reasoning shape

A `State` is a small immutable descriptor containing a root tree hash, parent
state IDs, attribution, intent, timestamps, and verification metadata
(`crates/object-model/src/object/state_core.rs:211-235`). Its ID is computed from
those fields (`crates/object-model/src/object/state_core.rs:509-523`). Trees are
sorted Merkle nodes whose entries point to blob or subtree hashes
(`crates/object-model/src/object/tree.rs:183-188`,
`crates/object-model/src/object/tree.rs:461-535`). Capture queues only blobs not
already present and reuses unchanged hashes
(`crates/repo/src/repository_tree.rs:518-532`,
`crates/repo/src/repository_tree.rs:614-655`).

Therefore a state is **logically a full snapshot but physically a
content-addressed, Merkle-shared snapshot**, not a patch and not a copied full
tree. A branch adds no new state. A capture of changed content adds the new blob
in full, changed ancestor trees, the small state/attachments, and oplog records.
The snapshot hot path disables inter-object delta search, though objects are
still compressed; aggressive GC can opt into delta search later
(`crates/objects/src/store/fs/fs_pack.rs:95-143`,
`crates/objects/src/store/fs/fs_pack.rs:330-344`). Identical content deduplicates;
incompressible changed binaries do not.

Raw reasoning is not the dominant default repository payload:

- a `Transcript` keeps the external JSONL source path so extraction can reread
  it; it does not embed the full transcript
  (`crates/ingest/src/transcript/types.rs:77-101`);
- a retained reasoning point is capped at 140 characters
  (`crates/ingest/src/reasoning.rs:49-73`);
- emitted points become context blobs and immutable superseding state
  attachments (`crates/ingest/src/reasoning_emit.rs:169-242`); and
- planned hosted timeline sync explicitly excludes raw args, shell commands,
  environment, stdout/stderr, and provider transcripts by default
  (`docs/adr/0039-versioned-agent-timeline-operations.md:20-28`).

There is a possible amplification edge: adding a point loads and rewrites the
whole context blob for that target, and old content roots/attachments remain
immutable (`crates/repo/src/repository_context.rs:43-63`,
`crates/repo/src/state_attachments.rs:67-90`). Repeated incremental appends to
one large target can therefore retain cumulative versions. Its real serialized
slope is **UNKNOWN**; a benchmark that repeatedly appends realistic notecards to
one target would settle it. Nothing measured here supports the hypothesis that
raw reasoning currently outweighs source content.

What dominates is workload-dependent. The idempotency cache dominates these
small real stores; timeline bookkeeping can dominate a read-heavy agent run
whose tools make few source changes; unique source content dominates a monorepo
or binary-churn workload. The current default does not retain enough raw
reasoning for transcripts themselves to dominate.

## Worst-case model

These are sizing models, not forecasts. Decimal GB is used for billing-like
figures; GiB is shown where useful. Every assumption is visible.

| Scenario | Arithmetic | Order of magnitude | Assessment |
|---|---|---:|---|
| Metadata-only fan-out loop | `100 branches/s × 86,400 s × 800 B` local logical; `× 4,096 B` loose-op allocation | 6.91 GB (6.44 GiB) logical/day; 35.4 GB (33.0 GiB) in loose-op blocks/day, plus ~4.2 GB derived data | **Trust-dangerous.** Hosted canonical bytes would be at least `8.64M × 316 B = 2.73 GB/day` before DB/index overhead. The current CLI slowdown is not a bound. |
| Retry/fan-out with captured changes | `10 attempts/s × 86,400 s × 25 KiB` newly compressed content | 22.1 GB (20.6 GiB)/day, plus timeline/state metadata | **Most dangerous.** Machine-driven, quiet, and directly billable. At 1 MiB/attempt the same rate is ~906 GB (844 GiB)/day. |
| Large binary churn | `100 MiB incompressible × 10 revisions/day × 250 workdays` | 250,000 MiB = 244 GiB/year | Capacity-dangerous, but content-driven and easier for a user to understand. Delta-free hot captures and incompressibility make this real. |
| Monorepo import | `1M files × 4 KiB` initial content + `100k commits × 10 changed files × 4 KiB` + `100k × 4 changed tree nodes × 4 KiB` + `100k × (0.5 KiB state + 0.3 KiB oplog)` | ~9.2 GiB, round to **10 GiB** plus pack/index overhead | Not intrinsically dangerous: a one-time, user-initiated import dominated by content. Actual file-size/history distribution and compression are **UNKNOWN**; benchmark a representative monorepo to settle it. |
| One ordinary agent-assisted year | `200 tool calls/day × 250 days × 2,405 B` + `10% captures × 25 KiB` + `25k notecards × ≤1 KiB estimate` | 115 MiB timeline + 122 MiB captured content + ~24 MiB reasoning estimate; low hundreds of MiB logical, plausibly under 1 GiB with allocation/metadata | Not dangerous under these assumptions. Exact notecard bytes and capture distribution are **UNKNOWN**. Read-heavy work makes timeline bookkeeping dominate tiny diffs, but not at surprising absolute scale. |

The genuinely dangerous scenario is not ordinary branching or a normal year. It
is an agent loop that combines high-rate `Retry`/`FanOut` creation with captures,
especially binary or generated output, because the user did not choose each
write and current retention has no finite ceiling. Large binary churn is also
large, but it is a familiar content-storage problem; monorepo import and ordinary
use are not the reason to weaken immutable navigation.

## Design options

| Option | What it preserves | What it costs / loses | Verdict |
|---|---|---|---|
| Reachability from named source refs (Git model) | Current source tips and their ancestors | Timeline branches are adjacent metadata, not source refs. Using source refs alone would erase precisely the retries/forks the product values. Timeline pins/roots do not exist yet. | Reject as a standalone policy. Useful only after first-class timeline roots exist. |
| Age-based expiry | Predictable maximum age and simple billing explanation | Age is unrelated to importance; old pinned audit/provenance can matter more than yesterday's retry. It breaks “navigate anywhere” automatically. | Reject for canonical history by default. Accept only for operational caches/raw diagnostics, or explicit org policy with tombstones. |
| Epoch/generation collapse | Bounded generations and fast lookup | Intermediate cursor positions disappear; epoch boundaries complicate undo, signing, and sync. | Future explicit-policy option, not v1. |
| Collapse a spent branch to branch point + tip | Topology, chosen outcome, and branch origin remain visible | Intermediate undo/redo and forensic tool sequence disappear. Source payload savings are small if another retained position references the same state. | Best future semantic compaction unit when paired with tombstones and explicit user/org policy. |
| Tombstones preserving identity while dropping payload | No opaque `NotFound`; audit can explain what existed, why it expired, and which policy acted | Tombstones themselves cost storage. A tombstone cannot replace content-addressed bytes under the same ID, and it cannot materialize a removed state. Signing proof semantics must be designed. | Required substrate before any canonical pruning, but insufficient alone. |
| Prune nothing; bound creation | Full current navigation and signatures remain intact; surprise is stopped before bytes exist | Does not reclaim already accepted canonical history. Requires admission control, accounting, and a clear pause/resume UX. | **Recommended v1.** It addresses the trust failure without spending the product promise. |

Deleting only rebuildable indexes/views is compatible with every option. Expiring
the idempotency response cache after its declared replay window is also separate
from repository history, though that window must be real and documented before
automatic compaction is wired.

## Proposal

### V1: finite creation envelopes, no canonical sweeper

Every agent run should begin with a finite creation envelope. Hosted admission
must atomically enforce the minimum remaining value across:

- `max_new_retained_bytes_per_agent_run`;
- `max_new_retained_bytes_per_repository_day`;
- `max_timeline_operations_per_minute`; and
- `max_retry_fanout_branches_per_run`.

The byte counters must charge the serialized/compressed bytes the service will
actually retain, including database/object overhead if that overhead is billed.
Operation counts are a loop signal, not a substitute for bytes. All agent writes
count; the `Retry` and `FanOut` reasons get a smaller diagnostic sub-budget but
must not be trusted as the only classifier.

At 50% and 80%, surface the used/remaining bytes and the dominant category
(content, timeline, reasoning/context, or operational cache). At 100%, pause the
run or reject the write **before** it becomes accepted billable state. The error
must be typed as a policy/budget stop and non-retriable until a human or org
policy raises the envelope; otherwise the error itself can drive another retry
loop. Show the estimated cost of the requested increase.

Exact defaults are **UNKNOWN** because pricing, included quota, database
overhead, and real workload percentiles are absent. Product telemetry from
opted-in internal workloads plus the Weft billing schema would settle them. The
safety invariant does not depend on the eventual numbers: every run has a finite
limit, and increasing it is an explicit user/org decision.

### Local and hosted policy

- **Local:** default to no canonical pruning. Agent-created writes still get a
  local run budget and warnings to prevent disk exhaustion, but the owner can
  raise or disable it because no hosted bill is incurred. Rebuildable timeline
  views/indexes may be discarded and regenerated. Operational cache expiry must
  state its retry-semantics window.
- **Hosted:** enforce byte and rate envelopes before acceptance. The user or org
  chooses retention and spend policy; Heddle may impose a service safety maximum
  but must not silently raise a billable limit. Rejected local-first operations
  remain visibly local-only rather than being pretended synced.
- **Sync:** once hosted accepts canonical history, it is not silently deleted by
  a new default. A later org retention policy must be versioned, visible at
  acceptance time, and produce tombstones/receipts consistently on every
  replica.

This divergence is deliberate: local disk ownership permits override; hosted
billing requires server-side admission. Neither side silently destroys accepted
history in v1.

### Future canonical retention: roots, then tombstones

Before a canonical sweeper can exist, add first-class named/pinned timeline
positions and a retention-policy record. Define the **protected navigation
closure** as:

1. every persisted current cursor for every thread;
2. every user/org named or pinned timeline position;
3. every in-flight materialization-recovery position;
4. every policy-protected branch tip and fork point;
5. every operation/state named by retained signing, attestation, or provenance
   records; and
6. transitively, the branch-parent and step-predecessor/successor records
   required to resolve those positions and perform their promised undo/redo,
   plus the full state → tree → blob and required attachment closure for every
   materializable position.

In v1, because canonical history is not pruned, “reachable” remains all accepted
timeline positions. In a future explicit retention policy, positions inside the
protected closure remain fully navigable/materializable. Positions outside it
may be reduced, but their IDs must resolve to typed tombstones; they must never
look like corruption or an accidentally missing file.

For a collapsed spent branch, the user should see: branch identity, parent and
branch point, retained tip, operation/step count, original content digests,
retention policy ID, actor, and deletion time, plus a clear “payload expired by
policy; this position cannot be materialized” status. “Not found” or a dangling
cursor is not acceptable.

## Signing and provenance consequences

Source-state signatures are detached attachments over the state's computed
hash, and verification loads the state before recomputing that hash
(`crates/repo/src/repository_signing.rs:104-145`,
`crates/repo/src/repository_signing.rs:215-267`). Signature evidence is
append-only and cannot supersede another signature
(`crates/repo/src/state_attachments.rs:22-56`). Removing a protected source
state or its required signature attachments would therefore make both timeline
materialization and provenance verification fail. The protected state closure
must never be pruned.

The collaboration-signing proposal says signatures cover canonical envelopes,
old bytes are not mutated to add trust, and later trust is expressed as a new
attestation (`docs/adr/0022-collaboration-operation-signing-staging.md:3-17`). A
retention tombstone must follow the same shape: it is a new signed retention
operation/attestation referring to the original ID and digest, not replacement
bytes installed under the old content address.

Timeline signing is not settled. The current timeline envelope contains schema,
kind, labels, and body but no actor or signature fields
(`crates/object-model/src/object/timeline.rs:296-303`), despite domain prose
saying timeline operations use native attribution (`CONTEXT.md:167-169`). It is
**UNKNOWN** whether a future signature verifies the full payload, a digest, or a
separate envelope. Ratifying the timeline signing schema and defining what proof
survives payload deletion are prerequisites to tombstone pruning.

## Mechanical safety assertion

Name the invariant **`TimelineNavigationClosurePreserved`**.

Before publishing any GC result:

1. Freeze the retention-root manifest and rebuild the pre-GC timeline view from
   canonical operations, ignoring the cached view.
2. Compute the protected navigation closure and record, for every member, its
   operation ID and canonical-byte digest, `{thread, branch, step, state}`
   resolution, materializability, state/tree/blob closure, and signature status.
3. Build the candidate retained store side by side; do not delete in place.
4. Rebuild its view from canonical retained operations/tombstones.
5. Assert that every protected root resolves to the identical branch, step, and
   state; every protected operation has identical bytes/digest; every promised
   state closure exists and hash-validates; every signature has the same
   verification result; and every policy-reduced ID resolves to its typed
   tombstone rather than `None`.
6. Assert that no persisted current cursor or recovery record targets a
   tombstone.
7. Abort before publication on the first mismatch, reporting the exact root and
   missing/mutated object, just as current GC refuses to claim success when the
   redaction invariant drops.

For v1's non-canonical cleanup, the assertion is stronger and simpler: the set
and bytes of **all** canonical timeline operations, oplog entries, states, and
their object closures are identical before and after; a fresh view rebuild is
equal. That makes view/index reclamation mechanically semantics-free.

## Explicitly not in v1

- No automatic age-based deletion of canonical timeline, oplog, state,
  reasoning/context, signature, or provenance records.
- No Git-style collection rooted only in source refs.
- No deletion of loose timeline operation files followed by index repair; the
  current reader would silently erase them from the view.
- No pruning of a state/tree/blob or signature attachment reachable from a
  retained cursor, pin, recovery record, or provenance anchor.
- No spent-branch collapse, generation squashing, or tombstone format until
  timeline roots, user-visible reduced-history UX, sync semantics, and signing
  proof are ratified.
- No identical automatic policy for local and hosted storage.
- No background sweeper as the first defense against a runaway agent.
- No claim that current debug-CLI throughput is a rate limit or that the sampled
  18 local stores represent hosted workloads.

## Open unknowns and how to settle them

| Unknown | What settles it |
|---|---|
| Hosted canonical/DB/index overhead and exact billable-byte definition | Audit the exact deployed Weft schema/config and benchmark serialized admission through billing metering. |
| Safe default byte/rate/branch envelopes | Internal opt-in telemetry for operations/run, compressed bytes/run, retry/fan-out percentiles, and pricing/quota input. |
| Real monorepo import and binary compression/delta distribution | Import representative large repositories into clean fixtures and record object classes and compressed pack deltas. |
| Incremental reasoning-context amplification | Benchmark repeated notecard appends to one target and inspect every retained context root/attachment. |
| Automatic edit-after-rewind behavior outside the OpenCode bridge | End-to-end audit/tests for every harness and external adapter. |
| Timeline signing and tombstone proof semantics | Ratify a timeline signing ADR and golden-test signature verification before/after a simulated payload-retention boundary. |
| Retention roots and pin UX | Product decision plus canonical model/codec design; no sweeper should infer them from source refs. |
