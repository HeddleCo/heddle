# Build prompt — Redaction primitive

> **Status:** historical — shipped in 0.2.3 (#12) and 0.2.4 (#14). This
> file is the original build brief, kept for context. Sections marked
> "for now" / "Single-repo only" / "verify" reflect the pre-implementation
> plan and are *not* current. The shipped behaviour:
>
> - Cross-replica propagation works via `proto::ObjectType::Redaction`
>   and the gRPC `RedactionTransfer` channel. Both `LocalSync` (peer-to-peer
>   file sync) and `HostedGrpcClient` (network sync) route incoming
>   redactions through `Repository::accept_wire_redactions`.
> - The wire path is fail-closed: unsigned, tampered, and
>   untrusted-key redactions are refused. Operators populate
>   `[redact] trusted_keys` per repo via `heddle redact trust add`.
> - `purge` propagates as a `Redaction` with `purged_at: Some(_)` and
>   replays via the same `accept_wire_redactions` chokepoint on each
>   replica. Bytes are dropped locally on the receiver, matching
>   `Repository::purge_blob` semantics. Packfile repack after wire-purge
>   remains an operator follow-up (`blob_remains_in_pack` flag).
>
> Current docs: `CHANGELOG.md` (0.2.3, 0.2.4 entries), the
> `commands_redact.rs` module doc, and `Repository::accept_wire_redactions`
> in `crates/repo/src/repository_redaction.rs`.

> Self-contained brief for the agent implementing `heddle redact apply` + `heddle purge apply`. Don't dispatch sub-agents for the implementation itself — touch the crates directly. Dispatch reviewers only at the end if you want a second opinion before commit.

## Goal

Add a first-class redaction primitive to Heddle so operators can scrub a sensitive blob (leaked credential, PII, etc.) from the repo *without* breaking the immutability story. The redaction itself is a signed, attributed Heddle operation — auditable in the oplog the same way merges are.

Two operations:

1. **`heddle redact apply <state-id> --path <file> --reason "..."`** — declares a blob redacted. Writes a `Redaction` record (a new object type) referencing the original blob's BLAKE3 hash. The state stays addressable; readers see the redaction notice in place of the original content. The blob bytes are still on disk at this stage — redaction is a *declaration*.
2. **`heddle purge apply <state-id> --path <file>`** — physically removes the underlying blob bytes from local store + the canonical remote store if configured. Workspace-owner capability only. The `Redaction` tombstone stays. A `Purge` oplog entry records who removed bytes, when, and the redaction it acted on.

Both operations are themselves Heddle actions: attributed (`Principal`), timestamped, signed (Ed25519, same as merges).

## Non-goals

- Rewriting state IDs. State IDs are content-addressed; they don't change. Redaction is *additive* — a new object that supersedes a read of the original.
- Removing the *reference graph*. If a state is reachable from a thread/marker, redaction doesn't touch reachability — it only changes what readers see when they materialize the file.
- Force-push semantics. There is no "rewrite history." A purged state's prior content is gone from bytes, but its `Redaction` record stays in the DAG forever.
- Recovering purged bytes. Once `purge` lands, the content is gone. Operators get one chance.

## Data model

Add to `crates/objects/src/object/`:

```rust
// New object kind alongside Blob, Tree, State, etc.
pub struct Redaction {
    pub redacted_blob: blake3::Hash,
    pub state_id: StateId,
    pub path: PathBuf,
    pub reason: String,
    pub redactor: Principal,
    pub redacted_at: Timestamp,
    pub signature: Ed25519Sig,
    pub supersedes: Option<Hash<Redaction>>,  // chain redactions if any
}

// New oplog entry kind alongside Capture/Merge/etc.
pub enum OpKind {
    // ... existing
    Redact(RedactOp),
    Purge(PurgeOp),
}
```

Materialization path (the place where `Repository::read_file` builds a working file from a state's tree) checks for an outstanding `Redaction` on the blob hash before returning bytes. If redacted, return a stub:

```
# this file was redacted on 2026-05-10T14:33Z by grace@example.com
# reason: leaked credential
# redaction: hd-r4d4c7e0 (signed)
```

The stub is text-only; it's safe to include in materialized worktrees, semantic diffs, and bridge-git exports.

## CLI surface

Add to `crates/cli/src/cli/cli_args/`:

```
heddle redact apply <state-id> --path <file> --reason "..." [--all-states]
  # --all-states: walk every reachable state and redact every
  # occurrence of the same blob hash. Default: just the named state.

heddle redact list                     # show all Redactions in repo
heddle redact show <redaction-id>      # show a specific redaction

heddle purge apply <state-id> --path <file> [--force]
  # Requires workspace-owner capability (Biscuit). Refuses unless
  # a Redaction already exists on the blob. --force confirms the
  # bytes-loss step.

heddle purge list                      # show all Purge oplog entries
```

Match the pattern in `cli/src/cli/cli_args/commands_review.rs` for arg structs + clap shape.

## Storage + replication

- The shared object/proto model stays in this repo: `crates/proto/src/object_graph.rs` defines `ObjectType::Redaction` and `crates/proto/src/native_pack.rs` carries the redaction pack handling. The local object store gets a new content type for `Redaction` objects. Same loose+packed strategy as Blob/Tree/State.
- Sync protocol (only the replication/server wire format moved — it now lives in the sibling **weft** repo at `crates/weft-server/src/server/grpc_hosted_impl/sync.rs`) needs to handle `Redaction` propagation. Pulling a state pulls any redactions on its blobs.
- `bridge git export` (in `crates/cli/src/bridge/git_export.rs`) must replace redacted-blob materialization with the stub when exporting to Git. The downstream Git commit then carries the stub, not the secret — even on push to GitHub.
- `heddle maintenance gc --prune` should NEVER GC a `Redaction` even if its referenced blob has been purged. The tombstone is structurally permanent.

## Signing

Reuse `crates/crypto/src/ed25519.rs` patterns. A `Redaction` signs over: `(redacted_blob_hash, state_id, path, reason, redactor_principal, redacted_at)`. Same `verify` path as `Merge::verify` so existing readers can audit redactions.

## Capabilities (Biscuit)

`heddle redact` — requires `redact:repo` capability. Default-granted to maintainers and above. Document in `.agents/agent-attenuation.md` if you touch that surface.

`heddle purge` — requires `purge:repo` capability. Default-granted to workspace owner only. Never delegate-able via attenuation (this is one of the few capability shapes that resists narrowing — the verifier rule should reject any attenuated purge token).

## Oplog

Both `Redact` and `Purge` write `OpLog` entries — same `record_op` pathway used by `Merge`. The oplog entry includes the operation's own state-id so `heddle undo` could theoretically reverse a `Redact` (back to "blob is visible" — but doesn't recover purged bytes). `Purge` is non-reversible by design; `heddle undo` on a Purge entry should fail with a clear message.

## Tests

Property tests in `tests/property/redact_purge.rs`:

1. `Redact` is idempotent — redacting a blob that's already redacted is a no-op (or returns a "supersedes" chain).
2. After `Redact`, reading the file from the state yields the stub. Reading the blob directly via hash still returns bytes (until `Purge`).
3. After `Purge`, reading via blob hash returns `BlobMissing(blake3::Hash, redaction_id)`. The state is still readable; its tree still resolves; only that one blob materializes as the stub.
4. `bridge git export` of a state with redactions exports the stub, never the underlying bytes — even if the bytes are still on disk pre-purge.
5. Signing roundtrip: a `Redaction` written, serialized, deserialized, verified.
6. Capability rejection: a maintainer-token attempting `purge` is denied at the verifier; the attempt is logged in the oplog as a refused operation.

Integration tests in `crates/cli/tests/`:
- `heddle redact apply` on a synthetic state with a faux-secret blob, verify the stub appears on `heddle show`.
- `heddle purge apply <state> --path <file> --force` after redact, verify bytes gone from the local store and an oplog entry is present.
- Replication: redact on node A, fetch on node B, verify B sees the stub without ever pulling the original bytes.

## Out of scope for this build

- UI surfaces in the sibling **tapestry** repo. The marketing copy is already in place; the hosted review surface will adopt the stub renderer via its existing materialize path once the backend ships.
- Recovery of purged bytes from backups. Operators should know they need their own backup discipline for irreversible operations.
- Cross-repo redaction propagation (e.g., if blob X is referenced from another Heddle repo via federation). Single-repo only for now.

## Acceptance criteria

A reviewer can answer "yes" to all of:

1. `heddle redact apply <state> --path <file>` runs, writes a `Redaction` object, the state's `read_file` returns the stub.
2. `heddle purge apply <state> --path <file>` requires `purge:repo` capability, removes the blob bytes from the local store, writes a `Purge` oplog entry.
3. Both operations are signed (Ed25519), and `heddle review show <state>` displays the redaction + purge in the verification strip alongside the merge signature.
4. `bridge git export` of a redacted state exports the stub, not the secret.
5. Property tests + integration tests pass.
6. `heddle maintenance gc --prune` does not collect `Redaction` or `Purge` objects even when their referenced blobs are gone.

## Where this lives

| What | Where |
|---|---|
| `Redaction` + `Purge` object kinds | `crates/objects/src/object/` |
| `RedactOp` / `PurgeOp` oplog entries | `crates/oplog/src/op_kind.rs` (check actual filename) |
| CLI args | `crates/cli/src/cli/cli_args/commands_redact.rs` (new) |
| CLI command handlers | `crates/cli/src/cli/commands/redact.rs`, `purge.rs` (new) |
| Materialization stub renderer | `crates/repo/src/materialize.rs` or wherever `read_file` lives — verify |
| Replication | sibling **weft** repo, `crates/weft-server/src/server/grpc_hosted_impl/sync.rs` |
| Bridge git stub-on-export | `crates/cli/src/bridge/git_export.rs` |
| Property tests | `tests/property/redact_purge.rs` |
| CLI integration tests | `crates/cli/tests/redact_purge.rs` |
| Capability rules | sibling **weft** repo, `crates/weft-server/src/biscuit/rules.biscuit` |
| `CLAIMS.md` (in the sibling **tapestry** repo) | Add `heddle redact apply` + `heddle purge apply` as shipped after this lands |

Once shipped, update the /security Scene 05 page in the sibling **tapestry** repo to drop its PLANNED label and reference the real CLI.
