# Heddle #1610: blob-lineage encoding design spike

Status: proposal for adversarial review. This document does not select a new
storage format and contains no production-path changes. The parallel
`spike/1610-delta-encoding` work supplies the measurements needed to make the
choice.

## Recommendation in one page

Do not add content deltas *inside* HCB2 yet. Keep existing HCB2 packs readable,
and use the measurement spike to decide whether explicit deltas beat the
current lineage ordering plus solid zstd by enough to pay for a more coupled
read path.

If they do, the first production design should be **independently indexed LMPK
blob records, choosing per revision between a full body and a bounded delta to
a materialized base** (Candidate 3). Prefer one-hop deltas initially. This uses
the pack format's existing `ObjectType::Delta`, base-id, resolved-size, index,
and decoder rather than putting a second seekable pack inside an HCB frame. It
also gives a cold content-id lookup a direct record seek and at most one base
read. Existing HCB2 packs remain on the dual-read path and are rewritten only
by an explicit/background repack.

The recommendation is conditional. Solid zstd may already capture nearly all
of the redundancy in a well-ordered lineage. If the measured size improvement
from explicit deltas is small, Candidate 0 is the right answer: keep the simple
encoding and spend complexity on read amplification (smaller/restartable
frames) or cross-frame dictionaries instead. A larger zstd window by itself is
not a promising first lever: the writer already asks for a 128 MiB window while
the normal compact-frame limit is 12 MiB
([compact_frame.rs:5-28](../../crates/pack/src/store/pack/compact_frame.rs#L5-L28),
[compact.rs:29](../../crates/objects/src/store/fs/repack/compact.rs#L29)).

The owner must decide four things before implementation:

1. Is the primary objective pack bytes, repack time, or cold single-blob read
   latency? No representation dominates all three.
2. What is the maximum acceptable cold-read amplification: a whole 12 MiB
   frame, a bounded chunk, or one full base plus one patch?
3. Must newly written packs remain readable by an older Heddle binary, or is
   new-reader/old-pack compatibility sufficient with protocol capability
   gating for new packs?
4. What measured improvement clears the complexity bar, including index and
   dictionary bytes rather than payload bytes alone?

## Scope and terms

This proposal is about the physical representation of immutable blob bodies in
settled/repacked storage and in packs transported as encoded pack records. It
does not change blob content ids, Git import semantics, loose-object capture,
or the HLR1/HDC1/NPK1 tree model.

In this document:

- **lineage** is the repacker's inferred ordering of versions of a path,
  including detected renames. It is a compression hint, not object identity.
- **full** means that a record can produce a blob without another blob body.
- **delta** or **patch** means copy/insert instructions against a named base.
- **solid** means several logical objects share one compression stream.
- **cheap point read** means work proportional to the requested blob and a
  small, explicitly bounded dependency set, rather than to every blob in its
  containing frame.

## What the current tree actually does

### HCB2 is full bodies plus length deltas

The uncompressed HCB2 layout is:

```text
HCB2 frame
+----------------------+-----------------------------------------------+
| "HCB2"               | 4-byte discriminator                          |
| blob_count            | u64 varint                                    |
| first_length          | u64 varint, omitted when count = 0            |
| later_length_deltas[] | signed varints relative to prior blob length  |
| bodies[]              | every blob body in full, concatenated         |
| checksum              | BLAKE3 of all preceding frame bytes           |
+----------------------+-----------------------------------------------+
```

`encode_blob_frame` only delta-encodes lengths; it appends every body unchanged
([blob.rs:20-47](../../crates/object-model/src/compact/blob.rs#L20-L47)). The
shared compact writer adds one whole-frame BLAKE3 checksum
([io.rs:57-60](../../crates/object-model/src/compact/io.rs#L57-L60)). The frame
is then zstd-compressed at level 19 with window log 27, long-distance matching,
and a zstd checksum; without the `zstd` feature, the bytes are stored raw
([compact_frame.rs:5-28](../../crates/pack/src/store/pack/compact_frame.rs#L5-L28)).
Thus HCB2 itself provides no content compression. In a featureless build its
body ratio is approximately 1.0x, apart from small framing overhead.

Repack does useful semantic work before compression. It walks states
newest-first, groups modified versions by path, joins exact and similarity
renames, sorts histories by extension/path, and deduplicates content ids in the
final order
([blob_lineage.rs:14-44](../../crates/objects/src/store/fs/repack/blob_lineage.rs#L14-L44),
[blob_lineage.rs:135-195](../../crates/objects/src/store/fs/repack/blob_lineage.rs#L135-L195)).
The writer accumulates that order into roughly 12 MiB frames and makes a
one-blob group an ordinary pack record rather than HCB2
([blob_writer.rs:21-68](../../crates/objects/src/store/fs/repack/blob_writer.rs#L21-L68),
[blob_writer.rs:99-125](../../crates/objects/src/store/fs/repack/blob_writer.rs#L99-L125)).
The grouping is therefore lineage-*ordered*, but frame boundaries can split a
lineage and unrelated adjacent histories can share a frame.

The verified context says Git delta chains are resolved before HCB2. The code
shape is consistent with that boundary: the lineage writer calls
`ObjectStore::get_blob` and receives full content before encoding
([blob_writer.rs:34-45](../../crates/objects/src/store/fs/repack/blob_writer.rs#L34-L45)).
Any Git-provided choice of base or delta instructions is not represented in
HCB2.

### The current point read is not O(1) in bytes or CPU

Every content id in an HCB2 frame is added to the LMPK index at the same
physical record offset
([streaming_builder.rs:542-623](../../crates/pack/src/store/pack/streaming_builder.rs#L542-L623)).
The index gets the reader to the frame in one lookup. It does not get the
reader to independently compressed bytes for the requested blob.

The cold read path is:

```text
content id
  -> LMPK index lookup
  -> shared record offset
  -> read/decompress the complete zstd payload
  -> verify the complete HCB2 checksum
  -> parse every length and every body
  -> hash every body to recover/verify its content id
  -> find requested id
  -> copy requested body into the returned Vec
```

The complete-payload decompression is at
[pack_reader.rs:613-685](../../crates/pack/src/store/pack/pack_reader.rs#L613-L685),
and HCB2 decoding computes a typed content hash for every full body at
[blob.rs:50-87](../../crates/object-model/src/compact/blob.rs#L50-L87). The
final selection is a linear search through the decoded objects
([pack_reader.rs:918-927](../../crates/pack/src/store/pack/pack_reader.rs#L918-L927)).
Even a blob-size query falls back to a full logical read when several blob ids
alias a shared offset
([pack_reader.rs:532-572](../../crates/pack/src/store/pack/pack_reader.rs#L532-L572)).

So the accurate claim is: **HCB2 has direct lookup to a bounded solid frame,
not direct lookup to one blob.** The recent-blob cache can hide that cost on a
warm read, and the pack manager deliberately prefers a duplicate hot record
over a solid-frame copy when both exist
([manager.rs:307-326](../../crates/pack/src/store/pack/manager.rs#L307-L326)).
After a settling repack retires old sources, a blob present only in the solid
frame pays the frame cost on its first read.

### The tree tiers are useful precedent, not a blob format to copy blindly

The loose tree hot path writes HLR1 materialized anchors and eligible HDC1
cumulative deltas. HDC1 is one hop to a materialized epoch anchor, refreshes at
128 revisions or 512 operations, and is rejected when it is not smaller than
HLR1
([tree_canonical.rs:20-36](../../crates/object-model/src/object/tree_canonical.rs#L20-L36),
[codec.rs:68-127](../../crates/objects/src/store/codec.rs#L68-L127)). Store reads
explicitly forbid an HDC1 anchor from itself being a delta
([fs_impl.rs:695-745](../../crates/objects/src/store/fs/fs_impl.rs#L695-L745)).
Those constraints are attractive for predictable blob reads, but trees have
sorted semantic entries and cheap operation deltas; arbitrary byte blobs do
not.

NPK1 is a separate settled-tree pack, deliberately distinct from capture-time
HLR1/HDC1. It has shared dictionaries, restartable records, indexes, an mmap
reader, and chunk verification
([npk1/mod.rs:1-39](../../crates/objects/src/store/fs/npk1/mod.rs#L1-L39)). It
selects an anchor or a smaller delta from bounded candidates and caps chains at
16
([npk1/builder.rs:289-355](../../crates/objects/src/store/fs/npk1/builder.rs#L289-L355));
resolution reads the indexed record and its bounded base chain
([npk1/reader.rs:245-275](../../crates/objects/src/store/fs/npk1/reader.rs#L245-L275)).
NPK1 stores trees and repeated tree-entry targets, not blob bodies. It is a
strong precedent for putting expensive lineage optimization in a settled pack
tier, but “dedup in NPK1” is not currently an alternative blob encoding.

## Design constraints

Any new representation should make these properties explicit:

1. **Content identity stays external and authoritative.** Reconstruct a blob,
   compute its typed content hash, and compare it with the requested id before
   returning it. A lineage hint can select candidates but cannot define
   identity.
2. **Base closure is local and immutable.** A pack must not depend on a loose
   base or a different pack that can be retired. Bases must precede dependents
   or be named in a cycle-checked graph installed atomically with them.
3. **Worst-case work is bounded.** Admit counts, offsets, output lengths,
   dictionary sizes, and chain depth before allocating. HCB2 already bounds
   compact counts and output; LMPK bounds delta depth at 50
   ([pack_reader.rs:27-29](../../crates/pack/src/store/pack/pack_reader.rs#L27-L29)).
   A blob-specific policy should normally be much tighter.
4. **Point-read cost is part of the format.** “Indexed” is insufficient unless
   the index identifies independently decodable bytes or a bounded dependency
   chain.
5. **Selection compares stored alternatives.** Compare
   `zstd(delta)+base-cost policy` with `zstd(full)`, including record/index
   overhead and a safety margin. The generic builder currently accepts a
   compressed delta when it beats the *raw* full body, then otherwise computes
   full compression
   ([pack_builder.rs:329-349](../../crates/pack/src/store/pack/pack_builder.rs#L329-L349));
   that test is not sufficient evidence that a delta beats solid or even
   independent zstd.
6. **Featureless behavior is deliberate.** Full+zstd loses all content
   compression when zstd is disabled. Explicit copy/insert deltas retain their
   redundancy removal without zstd, at the cost of base reads.

## Candidate summary

| Candidate | Physical unit addressed by the pack index | Cold single-blob work | Compression opportunity | Format change |
|---|---|---|---|---|
| 0. Current HCB2 full+solid zstd | Whole solid frame | Decompress/hash all blobs in frame | Long zstd history within ~12 MiB frame | None |
| 1. HCB3 seekable content deltas | Outer frame; inner directory selects records | Decode one bounded base chain | Explicit long-range reuse plus per-record zstd | HCB3; safe wire use also needs an LMPK version/capability story |
| 2. HCB3 independent zstd with dictionary/prefix | Outer frame; inner directory selects records | Decode dictionary/prefix plus one record | Shared learned tokens or one full prefix base | HCB3; same outer-version concern |
| 3. Independent LMPK full/delta hybrid | One LMPK record per blob | Seek target and bounded bases | Explicit delta where it wins, full zstd otherwise | None if the existing LMPK delta contract is reused exactly |

## Candidate 0: retain HCB2 full bodies plus solid zstd

### Layout and write path

Keep the current HCB2 layout shown above. Keep lineage ordering, the 12 MiB
frame target, full-body concatenation, one frame checksum, and outer solid
zstd. The LMPK index continues to alias every logical id to one shared record.

An optional tuning subcase is to alter frame size or insert more frame
boundaries at lineage boundaries. That changes compression/read amplification
without changing HCB2 bytes. It should be measured separately from a larger
zstd window.

### Read path and random access

Seek to the indexed shared LMPK record, decompress and verify the entire HCB2
frame, hash every body, and select the requested id. Lookup is cheap; a
single-blob cold read is not. Maximum ordinary amplification is approximately
the frame limit divided by requested size, although a single oversized blob is
stored as an ordinary record rather than a multi-blob HCB2 frame.

### Versioning and compatibility

No version change. New readers and old readers use the same `HCB2` magic and
LMPK v4 container. Existing packs are untouched.

### Expected characteristics

- Best simplicity and likely strong sequential compression for genuinely
  adjacent revisions.
- One zstd stream can exploit redundancy without materializing or choosing a
  semantic base graph.
- Repack CPU is dominated by lineage discovery and level-19 zstd rather than a
  pairwise delta search.
- Cold point reads, size probes, and sparse transfers pay whole-frame
  decompression and verification.
- Increasing the window above 128 MiB cannot help a normal 12 MiB frame.
  Increasing frame size may improve compression but directly worsens the read
  and transfer amplification this candidate already has.

### Measurement that resolves uncertainty

Measure current ratio and encode time against the same corpus/order with frame
targets such as 1, 4, 12, and 32 MiB. Also measure cold p50/p95 decompressed
bytes and latency for one requested blob, not only full-pack scans. If 12 MiB
solid is already within the agreed size budget and point reads are rare or
cache-hot, no new encoding is justified.

## Candidate 1: HCB3 seekable explicit content deltas

### Layout and write path

Define a new, internally indexed frame. One concrete sketch is:

```text
HCB3 frame
+---------------------------+-------------------------------------------+
| "HCB3"                    | new discriminator                         |
| flags / count / max_depth | bounded header varints                    |
| directory_length          | exact bounded directory size              |
| directory_checksum        | BLAKE3 of header + directory              |
| directory[count]          | sorted by content id                      |
|   content_id              | 32 bytes                                  |
|   result_length           | varint                                    |
|   kind                    | FULL or DELTA                             |
|   base_ordinal/distance   | present for DELTA, strictly backward      |
|   raw/stored length       | varints                                    |
|   payload offset          | varint                                    |
|   payload checksum        | checksum of independently stored payload  |
| payloads[]                | independently zstd-compressed full bodies |
|                           | or copy/insert patches                     |
+---------------------------+-------------------------------------------+
```

Payload order remains lineage order; directory order is content-id order for
binary search. The writer emits a materialized full anchor, tests a bounded set
of earlier candidate bases, and emits a delta only when its actual stored bytes
beat an independently compressed full record by an owner-selected margin.
Anchors may be periodic, but chain depth is the enforceable contract.

Do not retain HCB2's single checksum as the only inner integrity mechanism:
recomputing it would touch the entire frame. A checksummed header/directory plus
per-payload checksums permits partial reads. LMPK's container checksum still
protects the installed pack as a whole; staged publication should validate all
records before cutover, as current repack already does for staged packs
([staging.rs:126-154](../../crates/objects/src/store/fs/repack/staging.rs#L126-L154)).

### Read path and random access

The outer LMPK index still lands on the shared record. The HCB3 reader parses
the bounded directory, binary-searches the requested content id, follows
strictly backward base ordinals to a full anchor, decompresses only those
independent payloads, applies patches forward, then verifies the final typed
content hash.

This is cheap when the chain is shallow: O(directory search + requested output
+ patch bytes + bounded bases), not O(frame bytes). A one-hop policy reads at
most one full base and one patch. A depth-16 policy may save more bytes but can
turn one random read into 17 reads and reconstructions. Returning
`result_length` needs only the directory, not reconstruction.

This design requires a reader that can borrow slices from the mmap-backed pack
record. Calling today's generic `read_record_at_depth` first would copy the
whole raw HCB3 payload and forfeit much of the benefit.

### Versioning and compatibility

Use new magic `HCB3`; do not insert a version byte after `HCB2`, because an old
decoder interprets that position as `blob_count`. New readers should recognize
both magics, following the existing HCS1/HCS2 dual-magic precedent
([state.rs:12-18](../../crates/object-model/src/compact/state.rs#L12-L18),
[state_decode.rs:216-224](../../crates/object-model/src/compact/state_decode.rs#L216-L224)).

New-reader/old-pack compatibility is straightforward. Old-reader/new-HCB3 is
not: all ids still alias a shared LMPK record, so an old reader will route it to
the HCB2 decoder and reject the inner magic. That fails closed, but it looks like
an invalid compact frame rather than a precise “format too new” diagnostic. For
wire packs, either negotiate HCB3 support and fall back to HCB2, or bump the LMPK
container version so rejection is explicit before record interpretation (the
index version only needs to change if its own bytes or semantics change).
Current LMPK is magic `LMPK`, version 4
([pack/mod.rs:45-62](../../crates/pack/src/store/pack/mod.rs#L45-L62)), and its
reader currently rejects both older and newer container versions rather than
dual-reading them
([pack/shared.rs:142-170](../../crates/pack/src/store/pack/shared.rs#L142-L170)).
A bump therefore needs an intentional v4/v5 dual-reader, not only a constant
change.

### Expected characteristics

- Likely strongest representation for long files with small localized edits,
  especially when revisions fall across zstd frame/window boundaries.
- Base search and patch construction add repack CPU and working-set pressure.
- Binary blobs, generated files, compressed media, wholesale rewrites, and
  weak/branched lineage may fall back to FULL and gain nothing.
- Explicit deltas preserve some compression without the zstd feature.
- Format and reader complexity are high: this is a blob-specific pack inside a
  generic pack, with two indexes and two framing/checksum layers.

### Measurement that resolves uncertainty

Compare one-hop and depths 4/16 using the existing copy/insert codec, but score
against *stored full zstd*, not raw size. Report pack bytes including directory
and checksum overhead, base-search CPU/RSS, cold requested bytes/latency, and
the fraction of revisions choosing FULL. Break results out by text, source,
large generated files, already-compressed data, merges, and renames.

## Candidate 2: HCB3 independent zstd records with dictionary or prefix

### Layout and write path

This keeps every blob independently decompressible but supplies cross-object
context explicitly:

```text
HCB3 dictionary frame
+----------------------------+------------------------------------------+
| "HCB3" / mode flags       | DICTIONARY or PREFIX mode                |
| dictionary directory       | id, raw/stored lengths, checksum         |
| object directory[count]    | content id, result len, codec, dict/base |
|                            | id, payload offset/length/checksum        |
| dictionaries/full prefixes | bounded shared bytes                     |
| object payloads[]          | independent zstd frames                  |
+----------------------------+------------------------------------------+
```

Two variants should not be conflated in measurement:

- **trained dictionary:** train one or several small dictionaries from a
  repository/type sample; each blob record names one dictionary. There is no
  blob dependency chain.
- **prefix/patch-from compression:** retain a full lineage anchor and compress
  each descendant as an independent zstd frame using that anchor as its prefix.
  The reader needs the full prefix but does not apply a custom patch stream.

The writer compares independent ordinary zstd, dictionary/prefix zstd, and raw
for each body. Dictionaries are pack-local immutable objects and their bytes
count against the pack-size result.

### Read path and random access

Seek the outer record, parse the inner directory, map the requested payload,
load/cache the named dictionary or full prefix, and decompress one independent
zstd frame. A trained-dictionary read is approximately one dictionary plus one
blob. A prefix read is one full base plus one blob stream. There is no recursive
chain if descendants only name materialized prefixes. The directory carries
the logical length, so size lookup is cheap.

As in Candidate 1, the outer HCB3 record must stay mmap-sliceable; copying the
entire shared payload before inner lookup defeats the point.

### Versioning and compatibility

The same HCB3 dual-read and LMPK capability/version rules as Candidate 1 apply.
Dictionary identity, maximum size, training algorithm identifier (if it affects
reproducibility), zstd parameters, and checksum must be part of the format.
Decoding cannot depend on a machine-local or server-side dictionary cache that
is absent from the pack.

### Expected characteristics

- Better point isolation than solid zstd; dictionary bytes can be cached and
  reused across many reads.
- A trained dictionary may help numerous small homogeneous files that HCB2
  separates across frame boundaries. It is less likely to encode long
  arbitrary unchanged ranges as compactly as an explicit content delta.
- A full-prefix mode may approach explicit-delta compression while delegating
  parsing to zstd, but it still reads the prefix and may be sensitive to zstd
  API/version guarantees.
- Training adds repack CPU, nondeterminism risk unless inputs/order/parameters
  are fixed, and overhead when a repo is heterogeneous or small.
- A repo-global dictionary raises transfer amplification: a sparse object
  transfer must also include every referenced dictionary.

### Measurement that resolves uncertainty

Measure held-out dictionaries, not only training samples. Include dictionary
bytes, training time/RSS, decode latency with cold and warm dictionary caches,
and sparse-transfer bytes. Compare per-repo, per-extension, and per-lineage
dictionaries; reject a result that wins payload bytes only by minting too many
dictionaries.

## Candidate 3: independent LMPK full/delta records (recommended if warranted)

### Layout and write path

Do not emit a new shared blob-frame representation. Use one existing LMPK
record per blob:

```text
FULL blob record
  [content_id][Blob + result_size][stored_size][zstd(full) or raw]

DELTA blob record
  [content_id][Delta + result_size][stored_size]
  [tagged_base_content_id][zstd(copy/insert patch) or raw patch]

LMPK index
  content_id -> that blob's own record offset
```

This is already the generic pack-entry layout
([pack_builder.rs:406-415](../../crates/pack/src/store/pack/pack_builder.rs#L406-L415)),
and `ObjectType::Delta` is already part of LMPK v4
([pack/mod.rs:45-55](../../crates/pack/src/store/pack/mod.rs#L45-L55)). The
existing non-streaming pack builder performs a sliding-window base search with
a depth cap of 50
([pack_builder.rs:257-349](../../crates/pack/src/store/pack/pack_builder.rs#L257-L349)).
The settled writer should reuse the record/decoder contract, not necessarily
its current selection policy.

Concrete writer policy for a first implementation:

1. Preserve the existing semantic lineage order and rename handling.
2. Keep one materialized candidate base per active lineage, bounded by a memory
   budget and maximum blob size. Unrelated extension neighbors may be secondary
   candidates only if measurement justifies their CPU.
3. Encode descendants as one-hop patches to that full base. Refresh the base
   when the patch is not smaller than independently compressed full bytes by a
   safety margin, or after a bounded revision interval.
4. Store both base and dependent in the same replacement pack; require the base
   record to precede the dependent and validate the reconstructed content id at
   staging time.
5. Skip delta search for small, oversized, or already-compressed/incompressible
   bodies according to measured thresholds.

The current streaming builder explicitly has no delta encoding because it does
not retain recent bodies
([streaming_builder.rs:42-50](../../crates/pack/src/store/pack/streaming_builder.rs#L42-L50)).
Implementation would need a bounded-base `add_delta` path or a specialized
settled blob writer. That is production work outside this design spike; it does
not require changing the on-disk delta record if the existing codec is used.

### Read path and random access

The LMPK index seeks directly to the target record. For FULL, decompress only
that record. For a one-hop DELTA, read/decompress the patch, index-seek and
decompress one full base, apply the patch, enforce the recorded output bound,
and verify the final content id. The existing reader already follows tagged
base ids, decompresses delta payloads, and applies a depth limit
([pack_reader.rs:642-700](../../crates/pack/src/store/pack/pack_reader.rs#L642-L700)).

The result size remains in the target record's type+size header, so a size query
does not need to reconstruct the delta
([pack_reader.rs:567-572](../../crates/pack/src/store/pack/pack_reader.rs#L567-L572)).
This is the only candidate that obtains independently addressable blobs without
adding an inner directory and a new mmap-aware shared-frame API.

### Versioning and compatibility

If the writer uses the existing `DeltaEncoder` instruction format and LMPK
record semantics exactly, no new magic or container version is needed. New
packs are readable by current LMPK v4 readers, and new readers continue to read
old HCB2 shared records. Repack is the migration: old packs are not rewritten
on read.

If measurements motivate a new patch codec, base-reference kind, shared
dictionary, or a tighter rule that changes bytes rather than writer policy,
give it an explicit new object type/codec tag and bump/capability-gate LMPK.
Do not reinterpret existing `ObjectType::Delta` bytes.

### Expected characteristics

- Cheap and predictable point reads with one-hop bases; no whole-lineage frame
  decompression.
- Explicit deltas can remove redundancy even without zstd.
- More record and index overhead than HCB2, especially for many tiny blobs.
- Independent zstd loses solid-context wins for bodies where no selected delta
  is worthwhile.
- One-hop deltas may compress less than deeper chains, but they simplify base
  retention, sparse transfer, corruption isolation, and worst-case latency.
- Repack must retain/index candidate bases and pay delta-search CPU. The policy
  must be bounded so it does not recreate the snapshot-time regression that
  disables pairwise search for unrelated large blobs
  ([fs_pack.rs:366-400](../../crates/objects/src/store/fs/fs_pack.rs#L366-L400)).

### Measurement that resolves uncertainty

This is the most important comparison for the parallel spike: current HCB2
solid versus independently compressed FULL versus one-hop LMPK DELTA, all with
identical blob set and lineage ordering. Also show depth 4 as an upper-bound
experiment. Include total `.pack + .idx` bytes, repack wall/CPU/RSS, FULL/DELTA
selection rate, cold one-blob latency and bytes decompressed, full checkout,
and sparse-transfer behavior.

## Where the likely win is

There are three separate levers, and a single “compression ratio” obscures
them.

### 1. Redundancy model

Solid zstd and explicit content deltas both exploit repeated byte sequences.
Because the current input is already lineage-ordered and zstd uses LDM, it is
plausible that explicit deltas add little. It is also plausible that explicit
deltas win on long files, frame-boundary splits, or distances beyond the actual
available history. Only the same-corpus comparison resolves this; do not infer
an encoding win from Git pack ratios because Git's object order, base search,
window, and included objects differ.

If explicit deltas win materially, the win is in the redundancy model and
should be represented as independently addressable pack records. If they do
not, full+zstd is fine and HCB2's main defect is read granularity, not stored
bytes.

### 2. Context reach

The current 128 MiB zstd window exceeds a normal 12 MiB HCB2 frame, so a still
larger window cannot expose more history without also enlarging/reorganizing
frames. Better frame boundaries, a repo/type dictionary shared across frames,
or a settled pack-level dictionary can expose context that the current stream
cannot. Those are distinct experiments from changing the window.

Dictionary training is most credible for many small homogeneous files. It
must beat the cost of dictionary storage, training, transfer, cache misses, and
format coupling. A repo-global dictionary is not “free cross-repo dedup.”

### 3. Addressability and tiering

Even at identical pack bytes, replacing one 12 MiB solid decode with one blob
record can be the larger product win. Heddle's read path is by content id
([fs_impl.rs:499-520](../../crates/objects/src/store/fs/fs_impl.rs#L499-L520)),
and ordinary uncompressed pack records even have an mmap-backed zero-copy fast
path
([pack_reader.rs:468-522](../../crates/pack/src/store/pack/pack_reader.rs#L468-L522)).
A design decision should therefore report bytes touched per requested blob,
not label the existing layout “O(1)” and stop there.

Exact duplicate blob bodies already collapse to one content id before this
choice; lineage encoding targets near-duplicates. NPK1's target dictionary can
reduce repeated 32-byte references in trees, but it does not deduplicate blob
payloads. A future blob-specialized settled pack could learn from NPK1's
indexes, restart records, and chunk checksums. It should be a sibling tier
(for example, a separately specified blob pack), not an unversioned extension
of NPK1.

## Which layer should own lineage deltas?

### Not the loose blob tier

Loose blobs should remain independently readable full content, optionally with
their existing per-object compression wrapper
([codec.rs:51-60](../../crates/objects/src/store/codec.rs#L51-L60)). Capture may
see a content id without a trustworthy path/parent lineage, concurrent states
can branch, and loose bases can be pruned. Adding blob lineage to the loose
write path would make the everyday verb depend on history optimization and
would need a reverse-state story for missing/pruned bases.

HLR1/HDC1 can safely use a lineage hint at capture because their semantic tree
operations and materialized-anchor contract are explicit. That does not imply
arbitrary blobs should use HDC1-shaped loose storage.

### Prefer the settled pack layer

The repacker already owns the expensive information: state topology, tree
diffs, path histories, rename guesses, all full blob bodies, atomic staged
publication, and retirement of old packs. That is the natural place to choose
FULL versus DELTA and to guarantee base closure. It is also where HCB2 is
currently assembled
([compact.rs:37-57](../../crates/objects/src/store/fs/repack/compact.rs#L37-L57)).

Candidate 3 keeps lineage relationships in the existing LMPK record graph.
Candidates 1 and 2 move the same concerns into an HCB3 inner graph plus inner
index. If LMPK cannot eventually express a measured requirement such as shared
dictionaries or restartable subrecords, the cleaner escalation is an NPK1-like
settled blob pack with its own magic, version, index, and reader—not a growing
pack-inside-pack hidden behind HCB magic.

### Interaction with NPK1 and HLR1/HDC1 during repack

Keep the physical tiers independent:

- HLR1/HDC1 remain the capture-time/loose tree source. Repack materializes and
  validates tree values as it already does; blob encoding never names an HDC1
  body as a base.
- NPK1 continues to settle trees and tree-entry target references. It may
  inform candidate ordering via historical trees, but it does not own blob
  bytes.
- The generic replacement LMPK (or a future sibling settled blob pack) owns
  all blob bases and dependents needed to resolve its content-id index.
- Cutover publishes the complete replacement set before retiring its sources;
  no delta may reach across that cutover boundary.

## Versioning and rollout matrix

| Writer output | New reader reads old packs? | Current reader reads new output? | Safe rollout |
|---|---|---|---|
| HCB2 / LMPK v4 | Yes, unchanged | Yes | No migration |
| HCB3 inside LMPK v4 | Yes, with dual inner decoder | No for aliased blob records | Capability-gate wire; at-rest format-too-new policy; preferably explicit outer version |
| HCB3 inside LMPK v5 | Yes only if reader deliberately supports v4+v5 | No, but rejection is early/clear | Ship dual reader first, then writer; negotiate/fallback on wire |
| Existing LMPK v4 FULL/DELTA records | Yes, including HCB2 | Yes | Reader already exists; writer-policy rollout only |
| New patch/dictionary record codec | Only after dual reader | No | New tagged codec/type plus outer version/capability |

For any new format:

1. Land reader/admission/hostile-input tests before enabling the writer.
2. Keep HCB2 decode indefinitely or until an explicit storage migration
   contract says otherwise.
3. Write the new representation only during background/explicit repack, never
   as an incidental read repair.
4. Capability-negotiate hosted transfer. A server must not send HCB3 or a new
   LMPK record codec merely because it stores one internally.
5. Preserve logical content ids across rewrites and validate every staged
   reconstructed blob before atomic cutover.

## Required measurements and falsifiers

The parallel spike should make the following table possible for each corpus
and candidate:

| Metric | Why it decides the design |
|---|---|
| Total pack + index + dictionary bytes | Prevents payload-only wins |
| Ratio versus sum of unique full blob bytes | Comparable storage baseline |
| Repack wall time, CPU time, peak RSS | Bounds background cost and candidate search |
| FULL/DELTA/dictionary selection rate by blob class | Shows where the mechanism actually applies |
| Cold p50/p95/p99 one-blob latency and bytes read/decompressed | Captures random-access amplification |
| Warm-cache one-blob latency | Shows whether dictionary/base caching changes the result |
| Full checkout / sequential scan throughput | Protects the bulk-read case |
| Sparse transfer of one and 100 blobs | Captures base/dictionary/frame amplification |
| Featureless-build pack bytes and reads | Makes the no-zstd contract explicit |
| Corruption of header, directory, base id, patch, and output length | Proves fail-closed admission |

Minimum corpus slices should include long-lived source paths, renamed files,
branch/merge histories, many small homogeneous files, large generated text,
binary assets, already-compressed media, wholesale rewrites, and histories
whose lineage crosses the 12 MiB boundary. Report both representative weighted
results and worst cases; averages can hide the point-read regression.

Falsifiers for the recommendation:

- If Candidate 3 saves only a small owner-defined percentage over HCB2 while
  materially increasing repack CPU/RSS, retain Candidate 0.
- If one-hop loses most of the size win but depth 4 preserves it within the
  read budget, allow a small bounded chain rather than insisting on one hop.
- If trained dictionaries match explicit-delta bytes with cheaper writes and
  reads, prefer dictionary records—but only after a clean pack-level format is
  specified.
- If cold point reads are demonstrably irrelevant and full-pack scans dominate,
  solid HCB2's simpler and possibly smaller representation should win.
- If new packs must be readable by today's binaries, rule out HCB3 and any new
  codec; only existing LMPK FULL/DELTA writer policy is eligible.

## OPEN QUESTIONS for the owner

1. **Objective order:** rank stored bytes, repack CPU, repack memory, cold
   point-read latency, checkout throughput, and sparse-transfer bytes.
2. **Read bound:** choose a maximum base depth and decompressed-byte budget.
   Is one full base plus one patch acceptable? Is a 12 MiB frame acceptable for
   a 1 KiB blob?
3. **Compatibility direction:** must old binaries read new packs, or only new
   binaries read old packs? What hosted capability/fallback signal is
   available before encoded packs cross the wire?
4. **Win threshold:** what total-pack percentage and/or read-latency gain pays
   for a base graph? The threshold must include `.idx`, dictionaries, and
   duplicated anchors.
5. **Base policy:** one-hop epoch anchors, depth 4, or a larger bound? Should a
   historical parent always outrank a smaller non-parent candidate?
6. **Branch semantics:** may two branches share a compression base selected by
   content similarity, or should bases stay within inferred path lineage for
   predictability? This affects compression only, not identity, if validation
   is correct.
7. **Large blobs:** what maximum base size may the writer retain and a reader
   materialize? Should large/binary/already-compressed classes always stay FULL?
8. **Dictionary scope:** is deterministic dictionary training acceptable in a
   background repack? Per repo, extension, lineage, or not at all?
9. **Layer escalation:** if shared dictionaries win, extend LMPK with an
   explicit versioned facility or authorize a separate NPK1-like settled blob
   pack? Do not silently grow HCB2.
10. **Featureless builds:** is useful compression without `zstd` a requirement,
    or is raw HCB2 acceptable for that build profile?
11. **Access heat:** is there a durable, trustworthy signal for keeping recent
    blobs as hot independent records while settling cold history solid? Without
    one, a hot/cold duplicate tier adds space and policy without a defensible
    cutoff.
12. **Determinism:** must identical logical corpora produce byte-identical packs
    across machines/zstd versions, or only equivalent validated object sets?

Until these are answered with measurements, the smallest safe decision is to
keep HCB2 as the readable baseline and avoid minting HCB3. If explicit lineage
deltas clear the bar, use the pack's existing independently indexed delta
records first; they provide the clearest improvement in random access with the
least new format surface.
