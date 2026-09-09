# Original-author admission receipts

`heddle-thread-authority-admission-v2` is executor testimony about the first durable
receipt of an original signed Thread operation or dual-signed ownership claim. It proves that the host
verified that original account/agent's authority at that boundary. It does not
prove causal readiness, current authority, approval, publication consent or
landing eligibility. The original operation and its original signature remain
unchanged, including an agent's explicit attribution.

Trust comes from the receiver's independently enrolled hosted executor pin for
an immutable Spool genesis. An incoming receipt, author envelope, transport peer
or delivery credential cannot create that pin. The statement commits Spool UUID,
Spool genesis, Thread ID, typed operation-or-claim subject, original actor (account and optional
agent), original publisher, authority-envelope digest, executor and the executor's
first-admission timestamp. No author-supplied timestamp establishes prior rights.

The canonical record is named MessagePack in the Rust model's declared field
order, with exact re-encoding required. It is at most 2,048 bytes. Signing uses
Ed25519 over the UTF-8 format identifier, one NUL byte and the canonical record.
The SignedRecord wrapper requires exactly one signature whose public key equals
the committed executor. Verification also checks the original operation signature
and every operation/actor/scope/digest field against the original bytes.

ReplicationOperations carries optional `authority_admissions` sidecars identified
by their committed operation IDs. A receiver rejects duplicates and sidecars for
operations absent from that batch. Verified receipt bytes are retained atomically
with the original pending operation and relayed without regeneration. First
authority admission can precede causal readiness. Current delivery authorization
remains separate and must still succeed.

The device store and native replication driver now carry a typed
`ReceivedOperation` containing the original signature and optional exact receipt.
The operation, durable original-author marker and receipt columns commit in one
SQLite transaction. A failed parent insertion rolls them all back. The first
retained receipt is immutable: a later valid receipt cannot replace its timestamp
or bytes. Pending operations retain this evidence across restart while their
causal parents arrive. Indexed reads return original bytes, status and receipt in
one statement, and subsequent peers receive the same signed testimony.

An original already admitted locally can replay under its durable admission;
fresh authored work without a receipt still requires independently enrolled original
account authority. Current browser/device delivery remains freshly authenticated.
Receipts do not create publication consent: foreign sharing policies cannot expand
a private device's local export policy.

The local native driver and actual authenticated Iroh device endpoint exercise
foreign-account receipt delivery and exact re-export. The hosted issuer separately
must persist its own first-authority decision and exact receipt in the same
transaction; receiving a sidecar must never invent a historical hosted decision.

Verification at the portable checkpoint:

```text
cargo test --locked --offline -p heddle-thread-api --no-default-features \
  --features replication --lib authority_admission::tests -- --nocapture
test result: ok. 2 passed; 0 failed; finished in 0.26s
```

Four independent temporary omissions failed with test exit 101: executor pin
comparison, executor signature verification, original signature verification and
the decoder's pre-parse byte bound. Each failure reached its intended assertion;
all source guards were restored before the final passing run. These checks cover
the portable codec and binding layer. Durable storage and transport verification
are additional checks below.

Device storage and transport checks additionally cover a forced rollback after
receipt writes, pending receipt retention through restart, original parent arrival,
exact re-export through three native peers, and foreign original author delivery
through the actual authenticated Iroh router. No original author credential or
private signing key is required for subsequent relays.

Six storage/protocol omission controls each failed at their intended assertion:
executor enrollment, current delivery authorization, receipt persistence,
first-receipt immutability, duplicate sidecars and unmatched sidecars. All were
restored before the final suite checks. The real Iroh fixture also retains the
40-view capacity and cleanup checks and existing checkout/run operations.

Restored native SDK verification:

```text
cargo test --locked --offline -p heddle-thread-api --lib -- --nocapture
test result: ok. 55 passed; 0 failed; finished in 15.95s
```

The real device test `real_device_rpc_captures_without_weft_and_rejects_unowned_authority`
passed (1 passed, 0 failed, 74.53s), including foreign original authority receipt
admission and exact re-export. This run preceded the separate account-service
integration fixture; it does not claim coverage for those additional methods.

Restored repository suite (isolated temporary HEDDLE_HOME/HEDDLE_CONFIG):

```text
cargo test --locked --offline -p heddle-repo --lib thread_replication:: -- --nocapture
test result: ok. 26 passed; 0 failed; finished in 7.56s
```

The repository run also included the concurrently developed listing projection;
its registration and leaf are delivered in the separate account-service checkpoint.

The canonical subject distinguishes an original operation from an ownership claim. Both retain the same account/agent, exact envelope digest, independently pinned executor, and first-admission time. A receipt for one subject kind cannot authorize the other.
