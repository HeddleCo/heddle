# heddle-git-serialize

Engine-free Git content reconstruction from `heddle-object-model`. Normal
dependencies exclude `heddle-repo`, `sley-worktree`, `sley-hooks`,
`sley-sequencer` and `ureq`; run `python3 scripts/check-git-serialize-dependencies.py`
to verify the graph.

- `serialize_commit(&State, &ObjectId, &[ObjectId])` (also accepting a borrowed
  `CommitMetadata` view) uses resolved tree/parent
  IDs and preserves identity bytes, author/committer timestamps, `-0000`, ordered
  extension headers (including folded signatures), and raw message bytes.
- `serialize_tree(&Tree, ObjectFormat, resolve)` preserves recorded source
  order and raw modes, or writes canonical Git order/modes for native trees.
  The synchronous resolver supplies entry IDs; spoollinks are omitted as in
  local export.
- `serialize_blob(&Blob)` borrows the original content without copying.
- `object_id(ObjectFormat, kind, content)` hashes content with Git framing.
- `reconstruct_closure(&reader, &mut sink, NativeObjectId, ObjectFormat, limits)`
  asynchronously emits a root's trees/blobs and, for commits, parent history.
  Implement `ObjectReader` for your native store and `ObjectSink` for your
  verifier, object database or streaming pack writer. Both SHA-1 and SHA-256
  are supported.

The closure walk is iterative. It reads and emits each distinct native object
once per reconstruction. A native-to-Git memo (including active objects) is
bounded by `max_objects`; `ObjectSink::resolved` can reuse the caller's own
object table before reading or descending. `write_object` receives the native
ID for recording that binding. Retained native objects are bounded by
`max_depth` and `max_object_bytes`, and dependency/resolved IDs by
`max_pending_links`. `max_total_bytes` bounds cumulative serialized content;
tree sizes and conservative commit bounds are reserved before descending or
allocating output. Readers must enforce decoded object memory limits before
allocation and provide stable, content-addressed reads; sinks must consume or
persist bytes before returning. The caller owns access, redaction and
transactions. An error or cancellation may leave a partial output.
Lossy imports and spoollinks fail closed in closure reconstruction. Native
states without captured raw messages produce empty message bodies; projection
message/footer policy belongs to the caller.

The deterministic hosted seed is omitted from parent history because it has no
Git commit. A seed used as the root is rejected. Output size is checked before
commit/tree serialization; commit admission includes a conservative bound for
header and identity formatting.
