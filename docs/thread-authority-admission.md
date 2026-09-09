# Original-author admission receipts

`heddle-thread-authority-admission-v1` is executor testimony about the first durable
receipt of an original signed Thread metadata operation. It proves that the host
verified that original account/agent's authority at that boundary. It does not
prove causal readiness, current authority, approval, publication consent or
landing eligibility. The original operation and its original signature remain
unchanged, including an agent's explicit attribution.

Trust comes from the receiver's independently enrolled hosted executor pin for
an immutable Spool genesis. An incoming receipt, author envelope, transport peer
or delivery credential cannot create that pin. The statement commits Spool UUID,
Spool genesis, Thread ID, operation ID, original actor (account and optional
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

The model and signature codec are a staged contract checkpoint. The store,
replication sidecar dispatch and hosted issuer must be connected before claiming
foreign-account metadata admission is supported end to end.

Verification at the portable checkpoint:

```text
cargo test --locked --offline -p heddle-thread-api --no-default-features \
  --features replication --lib authority_admission::tests -- --nocapture
test result: ok. 2 passed; 0 failed; finished in 0.26s
```

Four independent temporary omissions failed with test exit 101: executor pin
comparison, executor signature verification, original signature verification and
the decoder's pre-parse byte bound. Each failure reached its intended assertion;
all source guards were restored before the final passing run. These are codec
and binding tests, not evidence that durable sidecar transport is connected yet.
