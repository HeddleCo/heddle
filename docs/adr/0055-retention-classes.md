---
status: accepted
---

# Retention classes

**Immutability is a promise about meaning, not bytes.** Heddle preserves the
identity, decisions, and provenance of meaningful history. Physical copies and
selected operational detail may be reclaimed under the rules below. Accepted
on 2026-09-28, this contract does not describe a shipped reachability collector.

## Decision

| Class | Boundary and authority | Reclamation rule |
| --- | --- | --- |
| **Source history** | A landed State, its Changes, content needed to materialize it, signed provenance, and state-bound review decisions/evidence. Landing promotes these records permanently, including ancestors and merged branches. The repository owner controls explicit repository deletion or a separately authorized purge; Weft verifies hosted authority. | Never age out and never collect merely because a ref moved or a pack was rewritten. Only explicit deletion/purge can erase them; shared objects survive while another retained record needs them. A targeted purge cannot invalidate landed descendants or review proof: removing their dependencies requires explicit deletion of the whole affected closure, with its IDs and verification loss disclosed before execution. |
| **Working history** | Unlanded captures, their source closure, undo/redo positions, and active or abandoned Threads. The user or an authorized organization sets a versioned, visible working-history policy. Landing promotes the relevant closure to source history first. | Default: retain without an age limit. An explicit policy may compact an abandoned Thread only after **30 days from the later of abandonment and its last capture**, with no live checkout, cursor, pin, review, pending sync, or recovery reference. A shorter policy may not skip that grace period. Keep a durable Thread/capture tombstone and fork/decision summary; materialization loss is explicit. |
| **Tool and agent activity** | Tool input/output detail, run summaries, and Agent Timeline operations, excluding source captures and evidence promoted above. The user or authorized organization sets the **Timeline Retention Policy**; hosted Thread policy is its narrower per-Thread bound. Raw forensic session material also requires its separate consent. | Default: retain recorded Timeline detail while its Thread exists; no age clock or Spool-wide expiry. A policy may `DISCARD` at admission or later, or `BOUNDED` from first admission; it may only shorten an existing deadline. Preserve identities, fork points, decisions, and an honest tombstone/status where the container survives. Thread/Spool deletion, `DeleteAccount`, and billing-lock deletion remove the container and its timeline metadata under their explicit lifecycle. |
| **Derived artifacts** | Indexes, physical pack layouts, caches, and rebuildable build outputs. The local store or hosted operator chooses representation and eviction. | Disposable at any time **only if** the retained canonical records can still be read or rebuilt. Repacking cannot change the retained object set. A build output promised as a durable deliverable must first be classified and billed as retained material, not treated as a cache. |

An attachment takes the strongest class of the record whose meaning it proves.
In particular, a signed State attestation, landing decision, review verdict, or
policy version used to authorize a landed State is source history, even if its
UI appeared in a Timeline. A discussion that certifies a retained decision
cannot be made unverifiable by compacting its underlying evidence. Raw tool
streams need not be retained merely because a compact, signed result cites them;
the result must say what was checked and whether the raw material expired.

## Reachability collection

`heddle gc` currently packs loose objects and Timeline operations and removes
redundant loose copies; it has no mark-and-sweep root set and does not reclaim
unreachable packed history ([heddle#1715](https://github.com/HeddleCo/heddle/issues/1715)).
A future collector must compute live canonical IDs before copying packs:

1. Root **all landed States**, not just current named refs, plus their Changes,
   signatures, review decisions, attestations, policy versions, and complete
   State → tree → blob/attachment closure. A moved ref does not unroot landings.
2. Root active Thread tips, named refs, persisted cursors, pins, fork points,
   undo/redo and review targets, pending publication, in-flight captures and
   materialization recovery, and every policy-protected abandoned position.
   Walk State parents, Change dependencies, Timeline predecessor/branch links,
   and the object and evidence closure required to navigate or verify them.
3. Treat newly written but not yet published objects as roots for **at least
   seven days**; serialize the mark/snapshot and pack retirement with captures
   and recovery. Unknown roots or incomplete traversal fail closed.
4. After a working-history policy deadline, write the attributed policy ID,
   affected IDs/digests, compaction time and typed tombstones before removing
   payloads. Allow **at least seven days** between that durable record and
   local pack reclamation, so readers and sync can observe the transition.
   Hosted Timeline expiry instead hides payload at its deadline and sweeps it
   with a durable replay fence, as the Thread Timeline design requires.
   Explicit account/Spool deletion and authorized purge use their own fenced
   lifecycle. Sweep only objects with no protected incoming reference; copy
   live objects to a new pack and retire old packs crash-safely. Never turn a
   retained cursor into a missing object.

Tombstones preserve enough identity and ancestry to explain the missing
materialization; they do not claim that discarded bytes can be reconstructed.
The collector must prove that a source State and its signed evidence/review
record remain jointly resolvable before retiring any pack.

## Product, authority, and billing

Replace PRODUCT_SPEC's unqualified **“Nothing disappears”** with: **“Meaningful
source history and its proof remain addressable until you explicitly delete or
purge them. Working and operational detail may be compacted only under a visible
retention policy; Heddle keeps an honest record of what was lost.”** A shorter
tagline may remain, but its adjacent explanation must carry this qualification.
`log`, `show`, `review`, and Timeline views should distinguish `retained`,
`compacted on DATE by policy P`, and `deleted/purged by ACTOR on DATE`; JSON
should expose the same status. Explicit container deletion may leave no
content-level tombstone, but the deletion action and its scope must be visible
before execution and in whatever audit record the lifecycle retains. No empty
page or `not found` may masquerade as expiry.

The classes apply locally and in Weft. The local repository owner is
authoritative for local retention and may retain a longer local copy; a signed
Spool/Thread policy from its authorized principal governs hosted admission and
expiry, with Weft verifying rather than minting authority. A local GC cannot
delete hosted history, and a hosted policy cannot silently erase an offline
local copy. Hosted Timeline default follows the owner's Thread-lifetime
decision; `ThreadRetentionPolicy.scrubbed_timeline` can only tighten it.
`RunPolicy.structured_retention_seconds` governs the device and cannot extend
hosted retention. Billing lock is an explicit, warned deletion lifecycle, not
ordinary reachability GC.

Hosted storage metering counts retained canonical source, working, and
operational payload bytes, plus durable tombstones, receipts and replay fences,
attributed to the billing owner. Retained build deliverables count as retained
material too. Only usage above the existing billing-policy allowance is charged.
Count a canonical object or physical pack
representation once, including its live encoded bytes; do not charge separately
for redundant loose copies, indexes, caches, or transient build outputs.
Expiring payloads stop counting after erasure, while retained metadata still
counts. Today's Weft usage rollup covers `object_location`,
`hosted_git_packs`, and `hosted_git_objects`; the proposed Timeline design calls
for adding its payload and fence accounting. Metering and lifecycle deletion
must cover every newly retained store before it ships.

## Migration

Pre-1.0, make a clean cut: classify existing records, add durable landing and
policy roots, and rebuild indexes/packs. Until classification and tombstone-aware
readers are complete, retain all existing canonical history and run only the
current representation cleanup. Do not infer expiry from old timestamps or
silently apply a new default to preexisting Threads.

## Decisions (2026-09-28)

1. **Working-history default:** Retain working history indefinitely by default.
   An explicit policy may compact an abandoned Thread only after the minimum
   30-day grace described above.
2. **Purge scope:** A targeted purge may not invalidate landed descendants or
   review proof. Removal requires explicit deletion of the affected closure,
   with its IDs shown before authorization.
3. **Hosted billing boundary:** Count durable tombstones, replay fences, and
   retained build deliverables as storage. Charge only overage under the
   existing billing policy; exclude rebuildable service overhead and transient
   outputs.

## Grounding

- `CONTEXT.md` defines Timeline Retention Policy and forbids silent age-based
  removal without one; `docs/design/timeline-retention-and-pruning-spike.md`
  identifies the required navigation roots and tombstone semantics.
- `crates/cli/src/cli/commands/gc.rs` and
  `crates/objects/src/store/fs/fs_pack.rs` show current pack/prune behavior.
- Weft `docs/design/2337-thread-timeline-pipeline.md` records the owner's
  Thread-lifetime default, tightening and lifecycle rules; Weft
  `crates/weft-registry/src/usage.rs` and
  `crates/weft-workers/src/storage_lifecycle.rs` show current metering and
  billing-lock/account deletion boundaries.
