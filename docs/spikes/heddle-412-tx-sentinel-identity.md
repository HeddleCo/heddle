# heddle#412 - should `Tx` and the transaction sentinel share identity?

**Status:** spike decision doc. No production code lands in this issue.

**Decision:** **B - keep them separate.** The in-process `Tx::transaction_id`
is a free-form, retry-stable idempotency key for an `AtomicMutation` commit. The
on-disk transaction sentinel's `transaction_id` is a durable daemon RPC handle,
generated as a canonical `OperationId` UUID and used to address a TOML state
machine on disk. They both eventually write `OpRecord::TransactionCommit {
transaction_id, .. }` strings, but the current code does not correlate them, and
forcing them to share one identity would couple two different lifetimes.

**Consequence for #359:** close #359 as a valid no-op. Future daemon transaction
work can add an explicit correlation field if it migrates the transaction service
onto `AtomicMutation`, but the generic in-process `Tx` and the durable sentinel
should not be unified as the same identity.

---

## Current facts

### `Tx::transaction_id`

`Tx` is an in-memory context threaded through one `AtomicMutation` execution nest.
The struct holds a borrowed `Repository`, checkout `scope`, `transaction_id:
String`, isolation precondition, nesting depth, rewind ledger, and committed flag
(`crates/repo/src/atomic/tx.rs:52-66`). `Tx::root` creates the root transaction at
depth 0 with a fresh ledger and a caller-supplied `transaction_id`; the comment
requires the id to be stable across retries of the same logical operation
(`crates/repo/src/atomic/tx.rs:68-90`).

The id is supplied by the mutation, not minted by `Tx`. `AtomicMutation` requires
`fn transaction_id(&self) -> String` and documents it as a stable idempotency key
derived from the operation's identity, never minted fresh per `execute`; only the
root mutation's key is used (`crates/repo/src/atomic/traits.rs:57-68`).
`execute` reads that key once, then `execute_attempts` clones the same string into
a fresh `Tx::root` on each retry attempt (`crates/repo/src/atomic/execute.rs:33-41`,
`:44-62`). That means the `Tx` object is ephemeral per attempt, while the
`transaction_id` string is intentionally stable across attempts of the same op.

The id's in-process use is exact-once commit dedup and diagnostics. `Tx::commit`
pushes an `OpRecord::TransactionCommit { transaction_id: self.transaction_id.clone(),
op_count }`, then calls `record_batch_exactly_once_if_unchanged` with the same
string and the transaction's isolation precondition (`crates/repo/src/atomic/tx.rs:333-352`).
The local oplog exact-once path scans the whole transaction index for that string
before appending (`crates/oplog/src/oplog/oplog_core.rs:448-460`, `:496-538`;
`crates/oplog/src/oplog/packed_oplog.rs:459-494`).

This is not an `OperationId` type. It is a `String`, and real current callers use
domain-specific strings that are not canonical UUIDs. For example,
`reserve_transaction_id` formats `op-id-reserve/{verb}/{operation_id}/{hash}`
(`crates/repo/src/operation_dedup.rs:268-275`), `thread start` formats
`thread-start:{scope}:{name}:...` (`crates/cli/src/cli/commands/thread.rs:1614-1625`),
and undo/redo formats `{action}:{scope}:gen{generation}:[...]`
(`crates/cli/src/cli/commands/undo_apply.rs:1638-1650`). These strings are valid
atomic idempotency keys, but they would fail the daemon sentinel's canonical UUID
parser.

`Tx` lifetime is also process-local. On normal errors the executor rewinds the
ledger before returning (`crates/repo/src/atomic/execute.rs:64-72`, `:94-126`).
On panic/unwind, `Drop` rewinds pre-commit staged effects and never appends to the
oplog (`crates/repo/src/atomic/tx.rs:416-432`). Nothing in `execute` or `Tx::commit`
writes or reads a `.toml` sentinel.

### On-disk transaction sentinel `transaction_id`

The local daemon transaction service persists a `TransactionSentinel` TOML file
at `<heddle_dir>/state/transactions/<transaction_id>.toml`; the sentinel is
written on `begin`, mutated by `commit` and `abort`, and read by `get_status`
(`crates/daemon/src/grpc_local_impl/transaction.rs:35-40`). Its serialized field
is `transaction_id: String`, alongside repo path, thread, message, state,
started-at metadata, base state, buffered op names, and abort reason
(`crates/daemon/src/grpc_local_impl/transaction.rs:41-58`).

The service-level identity is an `OperationId` UUID. `OperationId` is a UUID
newtype whose `new()` mints `Uuid::new_v4()` (`crates/objects/src/object/operation_id.rs:15-22`).
The transaction service validates every caller-supplied transaction id by parsing
it as `OperationId` and requiring the canonical string form before any sentinel
path is built (`crates/daemon/src/grpc_local_impl/transaction.rs:64-80`). The
path builder takes `&OperationId`, then formats `<id>.toml`
(`crates/daemon/src/grpc_local_impl/transaction.rs:76-85`).

`begin_transaction` generates this id with `OperationId::new()`, serializes it to
the sentinel and response, computes the sentinel path from the `OperationId`, and
saves the active sentinel (`crates/daemon/src/grpc_local_impl/transaction.rs:138-207`).
`commit_transaction`, `abort_transaction`, and `get_transaction_status` all parse
the request's transaction id as an `OperationId` first, then load the corresponding
sentinel (`crates/daemon/src/grpc_local_impl/transaction.rs:213-236`, `:284-307`,
`:350-358`). Tests pin both behaviors: begin creates an active sentinel
(`crates/daemon/src/grpc_local_impl/transaction.rs:480-500`), begin is idempotent
through `client_operation_id` and returns the same generated transaction id on
replay (`crates/daemon/src/grpc_local_impl/transaction.rs:726-745`), and the
sentinel path is derived from an `OperationId` file name
(`crates/daemon/src/grpc_local_impl/transaction.rs:748-765`).

The sentinel's durable lifecycle spans process death. `local_daemon` runs
`replay_active_transactions` before serving RPCs so an active sentinel from a
prior process is recovered before new begins race it
(`crates/daemon/src/local_daemon.rs:281-329`). Replay scans the sentinel directory,
aborts every parseable active sentinel by rewriting its TOML state to `aborted`,
drains `buffered_ops`, removes orphan temp files, and appends an audit
`TransactionAbort` record (`crates/daemon/src/transaction_replay.rs:1-42`,
`:185-205`, `:247-309`). Today replay treats `buffered_ops` as forensic metadata,
not a redo log (`crates/daemon/src/transaction_replay.rs:30-37`).

The daemon transaction service is not currently an `AtomicMutation` root. Its
commit path explicitly says it remains outside the same-thread CAS-on-commit
guarantee until it is migrated to the conditional oplog API or an
AtomicMutation-backed flow (`crates/daemon/src/grpc_local_impl/transaction.rs:245-254`).
It flips and saves the sentinel to `committed`, clears buffered ops, then
best-effort appends `OpRecord::TransactionCommit` with the sentinel's string id
using plain `repo.oplog().record_batch` (`crates/daemon/src/grpc_local_impl/transaction.rs:255-267`).
Abort follows the same pattern with `TransactionAbort`
(`crates/daemon/src/grpc_local_impl/transaction.rs:316-340`).

### Current correlation

There is no current code path that maps an in-process `Tx::transaction_id` to an
on-disk sentinel id.

The atomic executor reads the mutation's string id, creates `Tx::root`, applies,
and commits through the oplog exact-once API (`crates/repo/src/atomic/execute.rs:33-72`;
`crates/repo/src/atomic/tx.rs:333-352`). The daemon transaction service mints an
`OperationId`, writes a TOML sentinel, and later records commit/abort audit
records without constructing `Tx` (`crates/daemon/src/grpc_local_impl/transaction.rs:181-200`,
`:245-267`, `:316-340`). The only shared storage shape is that both can write an
`OpRecord::TransactionCommit { transaction_id: String, op_count }`
(`crates/oplog/src/oplog/oplog_types.rs:136-143`), but they reach that record from
separate APIs with separate identity rules.

The old #330 spike proposed a future bridge where the root `Tx` id would be the
same id written into the sentinel (`docs/spikes/heddle-330-atomic-mutation-primitive.md:1940-1945`).
That was explicitly optional follow-up work (`docs/spikes/heddle-330-atomic-mutation-primitive.md:2691-2695`).
The current shipped code has since made the split concrete: generic atomic
mutations accept non-UUID idempotency strings, while the transaction service uses
UUID sentinel handles.

---

## Same logical transaction, or different concerns?

They are different concerns that happen to share the word "transaction".

`Tx` represents a single in-memory mutation attempt. It owns the rollback ledger,
nesting depth, isolation precondition, and commit flag for code currently running
inside one process. Its `transaction_id` is not an object identity for the `Tx`
instance; it is the stable exact-once key presented to the oplog so a later retry
of the same logical operation dedups against the prior committed batch. One
logical operation can create several `Tx` instances across isolation retries, all
with the same key (`crates/repo/src/atomic/execute.rs:44-62`).

The sentinel represents a durable daemon transaction session and recovery point.
It exists before a commit, survives process death, is addressable by later
commit/abort/status RPCs, and can terminate as committed or aborted. Its id is a
public handle to a file-backed state machine. It is also separate from the gRPC
`client_operation_id`: `with_idempotency` uses the client operation id only to
reserve/replay the RPC result (`crates/daemon/src/grpc_local_impl/mod.rs:77-145`),
while `begin_transaction` mints a new transaction id as the resource it returns
(`crates/daemon/src/grpc_local_impl/transaction.rs:181-200`).

Could a daemon transaction be implemented as an `AtomicMutation` someday? Yes, but
that would still not make every `Tx::transaction_id` a sentinel id. It would mean
one specific daemon flow has two useful identifiers:

- `sentinel_transaction_id`: the durable `OperationId` handle clients use for
  begin/commit/abort/status and operators use for recovery files.
- `atomic_transaction_id`: the exact-once key used by the `AtomicMutation` root
  when it appends its committed batch.

For a future one-root-per-sentinel daemon flow, the second could be derived from
the first, for example `daemon-transaction:{sentinel_transaction_id}`. That is
correlation, not generic identity unification. The current generic atomic layer
must keep accepting operation-derived string keys such as `thread-start:...` and
`undo:...`, because those keys encode the retry semantics of those operations.

---

## What unification would buy

Shared identity has real benefits in the narrow daemon transaction case:

- A sentinel file and a `TransactionCommit` marker could be joined by one id while
  debugging a crash.
- Startup recovery could answer "did this active sentinel already commit?" by
  looking for the same `TransactionCommit` id in the oplog.
- Observability could show one user-visible id across RPC logs, sentinel files,
  replay reports, and committed oplog batches.
- A migrated daemon transaction service could avoid a class of split-brain bugs
  where the sentinel says one id and the atomic commit marker says another.

Those benefits are correlation benefits. They do not require making the generic
`Tx::transaction_id` type and the sentinel handle type the same abstraction.

---

## What unification would cost

The costs are larger than the benefits for the generic primitive.

First, it would break or distort existing atomic idempotency keys. The sentinel
path requires a canonical `OperationId` UUID before filesystem access
(`crates/daemon/src/grpc_local_impl/transaction.rs:64-85`), while current atomic
mutations use structured strings that are deliberately derived from operation
inputs (`crates/repo/src/operation_dedup.rs:268-275`;
`crates/cli/src/cli/commands/thread.rs:1614-1625`;
`crates/cli/src/cli/commands/undo_apply.rs:1638-1650`). Forcing these through
`OperationId` would either lose the operation-derived retry semantics or require a
secondary mapping layer, which is already an admission that the identities differ.

Second, it couples an in-process rollback abstraction to a durable on-disk RPC
format. `Tx` is created per attempt and dropped at the end of `execute`; the
sentinel is created on `begin`, lives across multiple RPCs and process restarts,
and is recovered by scanning a directory. Changing `Tx::transaction_id` would then
become an on-disk format decision, and changing the sentinel handle would become
an atomic executor API decision.

Third, unification would not by itself make the daemon transaction service atomic.
The service currently commits by saving the sentinel first, then best-effort
recording a `TransactionCommit` audit entry through plain `record_batch`
(`crates/daemon/src/grpc_local_impl/transaction.rs:255-267`). The code explicitly
marks that path as outside the AtomicMutation CAS-on-commit guarantee
(`crates/daemon/src/grpc_local_impl/transaction.rs:251-254`). Sharing the id would
improve grepability, but the correctness work would still be the larger migration
to a conditional oplog/AtomicMutation-backed flow.

Fourth, it increases ambiguity with `client_operation_id`. The daemon already has
an idempotency identity for RPC replay (`with_idempotency`) and a separate
transaction resource identity returned by `begin_transaction`
(`crates/daemon/src/grpc_local_impl/mod.rs:77-145`;
`crates/daemon/src/grpc_local_impl/transaction.rs:181-200`). Collapsing the
sentinel id into the generic atomic id would create a three-way naming collision:
client operation id, durable transaction handle, and exact-once commit key.

---

## Recommendation

Choose **B - keep separate**.

#359 should be closed as a no-op rather than implemented. The current code has a
clean separation:

- `Tx::transaction_id`: a stable, operation-derived string used by an ephemeral
  in-process mutation scope to dedup one durable oplog commit.
- Sentinel `transaction_id`: a daemon-generated `OperationId` UUID used as a
  durable RPC/file handle for transaction begin/status/commit/abort and startup
  recovery.

The actionable future shape is not identity unification. It is explicit
correlation only when a daemon transaction flow is actually migrated onto
`AtomicMutation`:

1. Keep the sentinel path and wire handle as `OperationId`.
2. Keep `AtomicMutation::transaction_id() -> String` for operation-specific
   idempotency keys.
3. If a sentinel-backed daemon commit becomes an `AtomicMutation`, derive that
   root mutation's atomic key from the sentinel handle in that specific flow, or
   persist an `atomic_transaction_id` field in the sentinel for observability.
4. Do not require unrelated atomic mutations to have sentinel files or UUID ids.

That gives maintainers the useful cross-reference when the daemon service needs
it, without turning every in-process transaction attempt into an on-disk
transaction resource.
