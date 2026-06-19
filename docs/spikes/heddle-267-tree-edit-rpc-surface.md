# heddle#267 - tree-edit RPC surface for FS-less operation

**Status:** spike decision doc. No production RPC code lands in this issue.

**Decision:** add a hosted **TreeEditService** whose read methods operate on
committed trees and whose write path stages content-addressed captures, then
publishes them through one server-side `CommitFromCaptures` transaction. The
wire idempotency key is reserved against the canonical request hash before
execution; the server then runs the corresponding `AtomicMutation` on the
client's behalf and commits through the existing dedup-first CAS-on-commit
oplog path.

This is not a filesystem proxy. The client never needs a local `.heddle/` or
worktree. Any operation that currently means "compare against the live checkout"
must instead name a committed state, a staged capture, or inline content.

---

## Grounding

The substrate needed for a no-local-FS surface is already present.

- `ObjectStore` is backend-neutral (`crates/objects/src/store/mod.rs`) and has
  Heddle-owned implementations for local files, memory tests, and shared
  dynamic dispatch. Hosted object-store backends live in Weft.
- `Repository` is generic over refs, oplog, and object store. The default CLI
  shape is `Repository<RefManager, OpLog, AnyStore>`, while hosted/server
  storage backends are assembled in Weft via `from_parts`.
- `Repository::build_store` now constructs the local `FsStore` variant.
- The current hosted proto has auth/control-plane RPCs, native push/pull, and a
  partial content surface. `ContentService` already exposes refs, states, trees,
  blobs, compare, and diff (`crates/grpc/proto/heddle/v1/service.proto:165-185`,
  `:1246-1371`).
- The local write primitive is no longer just a proposal. `AtomicMutation`
  requires a stable `transaction_id`, explicit isolation keys, staged forward
  work, rewind, and committed-output reconstruction
  (`crates/repo/src/atomic/traits.rs:45-113`). `Tx::commit` appends through
  `record_batch_exactly_once_if_unchanged`
  (`crates/repo/src/atomic/tx.rs:333-375`).
- CAS-on-commit exists in both the file oplog and the Postgres oplog. Both
  check for an existing `TransactionCommit { transaction_id }` first, then scan
  only the tail after the observed `head_id` for declared isolation-key
  conflicts before appending (`crates/oplog/src/oplog/oplog_core.rs:445-505`,
  `crates/oplog/src/oplog/pg_oplog.rs:282-419`).
- RPC idempotency has the missing cross-process guard: `OperationDedupStore`
  reserves `(operation_id, verb, request_hash)` before execution, returns
  replayed responses for matching retries, `InFlight` for concurrent matching
  callers, and `Conflict` for id reuse with a different body
  (`crates/repo/src/operation_dedup.rs:1-20`, `:353-455`). The local gRPC
  wrapper already maps that to `Replay`, `FailedPrecondition`, `Aborted`, or
  execute-and-record (`crates/daemon/src/grpc_local_impl/mod.rs:80-145`).

---

## Operation fit

| Operation | Current FS dependency | FS-less equivalent |
|---|---|---|
| `CaptureFromContent` | There is no exact local public primitive with this name. The closest shipped path, `snapshot_tree_with_attribution_profiled`, already accepts a supplied `Tree` and avoids walking the worktree, but it still opens a local repo, uses the checkout lane, and publishes `HEAD` after the atomic commit (`crates/repo/src/repository_snapshot.rs:580-635`). `snapshot_with_attribution_profiled` and `capture_thread_from_disk` are explicitly worktree walkers (`crates/repo/src/repository_snapshot.rs:501-578`, `crates/repo/src/repository_thread_materialize.rs:150-220`). | Clean as an RPC if "capture" means content staging, not publication. The request supplies a base state/tree plus file edits or full tree content. The server stores blobs/trees through its `ObjectStore`, returns a `capture_id` and root tree hash, and does not move a thread. Orphaned staged objects are acceptable and GC-able. |
| `CommitFromCaptures` | The shipped snapshot commit is now an `AtomicMutation`, but its local wrapper still takes a repo write lock, reads merge state, resolves local `HEAD`, uses `op_scope`, and refreshes materialized refs/manifests (`crates/repo/src/repository_snapshot.rs:503-577`). | Needs design, not plumbing. The RPC targets a hosted thread, consumes one or more capture ids, creates a new `State`, and publishes the thread through server-side `AtomicMutation`. It declares `IsolationKey::Thread(target_thread)`, uses the wire idempotency key and request hash, and returns the committed state or a structured conflict. |
| `StatusForThread` | Current `status` is a CLI view over local repo open, current state, operation state, git-overlay health, remote tracking, worktree index/fsmonitor, materialized-thread manifests, and thread summaries (`crates/cli/src/cli/commands/status.rs:422-500`, `:631-760`). | Split the contract. `StatusForThread` without content reports committed hosted thread state, target/base relation, remote tracking known to hosted, and optional policy/actor facts. Dirty-worktree fields exist only when the request supplies `compare_tree`, `capture_id`, or inline edits. No server claim about a client filesystem. |
| `DiffForThread` | State-to-state diff is already object-store based (`Repository::diff_trees`, `crates/repo/src/repository_diff.rs:11-16`; tree diff uses `ObjectStore`, `crates/objects/src/object/tree_diff.rs:15-29`). Default CLI diff still means HEAD vs live worktree unless `to` is supplied (`crates/cli/src/cli/commands/diff/diff_compute.rs:56-228`). | Clean for committed refs and staged captures. The RPC requires `from` and either `to_ref`, `to_capture_id`, or inline `to_tree`. A missing `to` should mean "thread head versus first parent" or be rejected; it must not imply an invisible worktree. |
| `LogForThread` | `cmd_log` opens a local repo and resolves CLI flags, but history traversal is over refs, states, and object-store graph data (`crates/cli/src/cli/commands/log.rs:128-183`; `Repository::query_history`, `crates/repo/src/repository_history.rs:132-190`). Path filters may diff trees, still through `ObjectStore`. | Clean. `LogForThread` names repo + thread/ref + filters and returns state summaries. It can reuse the content service's state summary shape, with path-filter cost called out. |
| `PushToRemote` | Current push is a local orchestration command: it runs local hooks, switches between git-overlay/local/native network paths, opens local targets, enumerates local object closures, and invokes `HostedGrpcClient::push` for hosted network remotes (`crates/cli/src/cli/commands/remote/mod.rs:153-420`, `:1091-1205`; `crates/client/src/grpc_hosted/sync.rs:159-330`). | Needs a hosted bridge contract. Native hosted-to-hosted push can reuse repo-sync semantics internally. External Git push must delegate to the server-side subprocess fetch/push work from #264, using server-held remote credentials and returning a job/result. It must not run client-local hooks or inspect client-local Git. |

The key product decision is that FS-less "status" and "diff" are about a
named hosted tree unless the caller supplies an overlay tree. This avoids
smuggling an implicit checkout back into the API.

---

## Write concurrency and transactions

### Chosen model

`CommitFromCaptures` is the only write-side commit point. The server runs a
Heddle mutation equivalent to `SnapshotMutation { source: SuppliedTree, head:
Attached(target_thread), ... }`, but with hosted-thread inputs instead of local
`HEAD`/`op_scope`.

The RPC layer wraps the mutation in two guards:

1. **Wire idempotency reservation.** `client_operation_id` is required for
   `CommitFromCaptures`. The server computes a canonical protobuf request hash
   and reserves `(operation_id, "TreeEditService.CommitFromCaptures",
   request_hash)` before any object/ref/oplog work. Matching completed retries
   return the cached response. Matching in-flight retries return `ABORTED`.
   Mismatched body reuse returns `FAILED_PRECONDITION`. This is the RPC
   equivalent of the eager op-id reservation from #358.
2. **Server-side `AtomicMutation` CAS.** The mutation's stable
   `transaction_id` is derived from the method name, repo id, target thread,
   `client_operation_id`, and canonical request hash. The mutation declares
   `IsolationKey::Thread(target_thread)`. The executor captures `oplog.head_id`
   before apply, stages object/state records, then commits through
   `record_batch_exactly_once_if_unchanged`. Dedup happens before isolation
   scanning, matching the shipped #392 behavior.

This gives the RPC client the same useful guarantees as local `AtomicMutation`:
exactly-once replay for one logical request, no double execution under
cross-process retries, and per-thread isolation against intervening hosted
commits. It also keeps the authority in one place: the server, not the client,
chooses the observed `head_id`, appends the oplog batch, and publishes the
thread materialized view.

### Transaction boundaries

`CaptureFromContent` is durable staging, not a transaction. It may be retried
with the same `client_operation_id`; a replay returns the same `capture_id`.
Its side effects are content-addressed objects and a small capture manifest.
If the client abandons the capture, the objects are unreachable garbage.

`CommitFromCaptures` is a single RPC transaction. It consumes capture manifests,
reads the current target thread, writes a new state, and appends the
transaction commit marker in one server-side `AtomicMutation`. There is no
long-lived "begin transaction / edit / commit" lock across RPC calls in the
first surface. Multi-call edit sessions can be added later as leases, but they
are not required to make FS-less agents useful and would create server resource
lifetime questions the content-addressed capture model avoids.

The commit request should include an optional `expected_head_state`. When set,
the server rejects before staging if the hosted thread no longer points there.
When omitted, the server parents the new state to the current thread head it
reads inside the mutation attempt. In both cases, CAS-on-commit remains the
lost-update guard; `expected_head_state` is a caller-facing semantic precondition,
not the isolation mechanism.

### Error recovery

- `OK` with `replayed = false`: the server executed and recorded the response.
- `OK` with `replayed = true`: the same completed operation id/body was retried;
  no mutation re-executed.
- `ABORTED`: same operation id/body is currently in flight, or the server
  exhausted bounded CAS retries. The client may retry the identical request
  with the same `client_operation_id`.
- `FAILED_PRECONDITION`: operation id was reused with a different request body,
  `expected_head_state` did not match, or a non-retryable policy check failed.
- `UNAVAILABLE` / deadline after the request was sent: retry the identical
  request with the same `client_operation_id`. The dedup record or oplog
  transaction marker determines whether the retry replays or completes.

`CaptureFromContent` failures after object writes do not need rollback. The
server should either return a capture manifest or leave only unreferenced
objects. `CommitFromCaptures` failures before commit run `Tx::rewind_all`; once
the oplog commit marker exists, retry reconstruction must return the original
state id, as local `AtomicMutation::reconstruct_committed_output` already
requires.

---

## PushToRemote and #264

`PushToRemote` should be a bridge operation over hosted repository state, not
an extension of `CommitFromCaptures`.

For native Heddle remotes, the server can perform the same object-closure and
ref-update work that `RepoSyncService.Push` currently exposes on the wire, but
with both repositories resolved server-side. The client names source repo,
state/thread, target remote, and force policy; it does not upload local objects.

For Git remotes, `PushToRemote` should call the server-side subprocess
fetch/push runner from #264. The RPC contract should enqueue or synchronously
run a bridge job that:

- materializes the needed state into the server-side bridge workspace owned by
  weft, not the caller;
- fetches first when required to classify fast-forward/divergence;
- pushes using server-held remote credentials and hosted audit identity;
- reports stdout/stderr summaries, remote ref updates, and retryable vs
  non-retryable failure classification.

This bridge job is outside the tree-edit transaction. If a client wants to
commit and push, it first calls `CommitFromCaptures`; after that response names
the committed state, it calls `PushToRemote` with a separate
`client_operation_id`. A push failure never rolls back the committed Heddle
state. That matches current CLI behavior: push is already a remote side effect
after local state selection, not part of the capture commit.

Local `pre_push`/`post_push` hooks do not translate to FS-less RPC. The first
surface should omit them or replace them later with hosted policy hooks. Running
client-local hook scripts would reintroduce the kernel/FS dependency the spike
is removing.

---

## Proto sketch

Illustrative only; tags and imports would be reconciled when production proto
work begins.

```proto
service TreeEditService {
  rpc CaptureFromContent(CaptureFromContentRequest) returns (CaptureFromContentResponse);
  rpc CommitFromCaptures(CommitFromCapturesRequest) returns (CommitFromCapturesResponse);
  rpc StatusForThread(StatusForThreadRequest) returns (StatusForThreadResponse);
  rpc DiffForThread(DiffForThreadRequest) returns (DiffForThreadResponse);
  rpc LogForThread(LogForThreadRequest) returns (LogForThreadResponse);
  rpc PushToRemote(PushToRemoteRequest) returns (PushToRemoteResponse);
  rpc GetTreeEditOperation(GetTreeEditOperationRequest) returns (TreeEditOperation);
}

message TreeEditRepo {
  string repo_path = 1; // hosted namespace/repo path
}

message TreeBase {
  oneof base {
    string ref = 1;          // thread, marker, or state spec
    string state_id = 2;     // full hd-* string
    bytes tree_hash = 3;     // raw ContentHash bytes
    bool empty = 4;
  }
}

enum TreeEditFileMode {
  TREE_EDIT_FILE_MODE_UNSPECIFIED = 0;
  TREE_EDIT_FILE_MODE_REGULAR = 1;
  TREE_EDIT_FILE_MODE_EXECUTABLE = 2;
  TREE_EDIT_FILE_MODE_SYMLINK = 3;
}

message FilePut {
  string path = 1;
  TreeEditFileMode mode = 2;
  oneof content {
    bytes inline_bytes = 3;
    bytes existing_blob_hash = 4;
  }
}

message FileDelete {
  string path = 1;
}

message TreeEdit {
  oneof edit {
    FilePut put = 1;
    FileDelete delete = 2;
  }
}

message CaptureFromContentRequest {
  TreeEditRepo repo = 1;
  TreeBase base = 2;
  repeated TreeEdit edits = 3;
  // If true, edits are the complete desired tree from `base = empty`;
  // otherwise they are applied as an overlay to `base`.
  bool full_tree = 4;
  // Idempotency; required for clients that need safe retries.
  string client_operation_id = 15;
}

message CaptureFromContentResponse {
  string capture_id = 1;
  bytes root_tree_hash = 2;
  string base_state_id = 3;
  repeated string changed_paths = 4;
  bool replayed = 5;
}

message CommitAuthor {
  string principal_name = 1;
  string principal_email = 2;
  optional string agent_provider = 3;
  optional string agent_model = 4;
  optional string agent_session_id = 5;
}

message CommitFromCapturesRequest {
  TreeEditRepo repo = 1;
  string target_thread = 2;
  repeated string capture_ids = 3;
  optional string expected_head_state = 4;
  optional string intent = 5;
  optional float confidence = 6;
  CommitAuthor author = 7;
  // The RPC transaction id. Server reserves it against this request's
  // canonical hash, then derives the internal AtomicMutation transaction_id
  // from method + repo + target_thread + this id + request hash.
  string client_operation_id = 15;
}

enum CommitResultKind {
  COMMIT_RESULT_KIND_UNSPECIFIED = 0;
  COMMIT_RESULT_KIND_COMMITTED = 1;
  COMMIT_RESULT_KIND_REPLAYED = 2;
  COMMIT_RESULT_KIND_CONFLICT = 3;
}

message CommitConflict {
  string kind = 1; // "expected_head" | "isolation"
  string expected_state = 2;
  string actual_state = 3;
  string isolation_key = 4;
  uint64 since_head_id = 5;
  uint64 conflicting_entry_id = 6;
}

message CommitFromCapturesResponse {
  CommitResultKind kind = 1;
  string state_id = 2;
  bytes root_tree_hash = 3;
  string parent_state_id = 4;
  CommitConflict conflict = 5;
}

message Treeish {
  oneof value {
    string ref = 1;
    string state_id = 2;
    string capture_id = 3;
    bytes tree_hash = 4;
  }
}

message StatusForThreadRequest {
  TreeEditRepo repo = 1;
  string thread = 2;
  optional Treeish compare_tree = 3; // absent = committed-thread status only
}

message PathSet {
  repeated string modified = 1;
  repeated string added = 2;
  repeated string deleted = 3;
}

message StatusForThreadResponse {
  string thread = 1;
  string head_state = 2;
  string base_state = 3;
  string target_thread = 4;
  string coordination_status = 5;
  PathSet changes = 6; // empty unless compare_tree supplied or thread delta exists
  bool compared_to_supplied_tree = 7;
}

message DiffForThreadRequest {
  TreeEditRepo repo = 1;
  string thread = 2;
  Treeish from = 3;
  Treeish to = 4;
  bool include_patch = 5;
  bool include_semantic = 6;
}

message DiffForThreadResponse {
  string from_state = 1;
  string to_state = 2;
  repeated FileDiff files = 3; // reuse existing ContentService shape
  CompareSummary summary = 4;
  optional string patch = 5;
}

message LogForThreadRequest {
  TreeEditRepo repo = 1;
  string thread = 2;
  uint32 limit = 3;
  optional string since_state = 4;
  repeated string paths = 5;
  optional string agent_model_substring = 6;
}

message LogForThreadResponse {
  repeated StateSummary states = 1; // reuse existing ContentService shape
}

message PushToRemoteRequest {
  TreeEditRepo repo = 1;
  string source_thread = 2;
  optional string state_id = 3; // empty = source_thread head
  string remote = 4;
  bool force = 5;
  bool wait = 6; // false = enqueue and return job id
  string client_operation_id = 15;
}

enum TreeEditOperationState {
  TREE_EDIT_OPERATION_STATE_UNSPECIFIED = 0;
  TREE_EDIT_OPERATION_STATE_QUEUED = 1;
  TREE_EDIT_OPERATION_STATE_RUNNING = 2;
  TREE_EDIT_OPERATION_STATE_SUCCEEDED = 3;
  TREE_EDIT_OPERATION_STATE_FAILED = 4;
}

message PushToRemoteResponse {
  string operation_id = 1;
  TreeEditOperationState state = 2;
  string pushed_state = 3;
  repeated string updated_refs = 4;
  string error = 5;
  bool replayed = 6;
}

message GetTreeEditOperationRequest {
  TreeEditRepo repo = 1;
  string operation_id = 2;
}

message TreeEditOperation {
  string operation_id = 1;
  TreeEditOperationState state = 2;
  string kind = 3; // "push_to_remote" initially
  string result_json = 4;
  string error = 5;
}
```

---

## Proposed impl issues

### 1. Implement read-side tree-edit RPCs for hosted threads

Scope: add `StatusForThread`, `DiffForThread`, and `LogForThread` over hosted
repository refs/states/trees. Keep the contract explicit: committed tree by
default, optional caller-supplied compare tree/capture for dirty overlays, no
implicit filesystem status. Reuse existing `ContentService` shapes where they
fit.

Blocked by #267.

### 2. Implement write-side content capture and commit RPCs

Scope: add `CaptureFromContent` staging plus `CommitFromCaptures` publication.
Require `client_operation_id` for commit, reserve it against the canonical
request hash, and run a server-side `AtomicMutation` with per-thread
CAS-on-commit isolation. Return replay/conflict details as structured RPC
responses/status codes.

Blocked by #267.

### 3. Implement hosted `PushToRemote` bridge RPC

Scope: add an RPC that pushes a hosted thread/state to a configured remote
without client-local Git or filesystem hooks. Native hosted remotes can reuse
repo-sync internals; Git remotes delegate to the #264 server-side subprocess
fetch/push runner and expose an operation/job result.

Blocked by #267.
