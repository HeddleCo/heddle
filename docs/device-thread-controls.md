# Portable Thread control admission

Native Thread metadata uses an independent causal register for name, intent,
lifecycle, sharing policy, and each review UUID. An update observes every head of
its own field. Concurrent candidates remain explicit; unrelated fields compose.
Changing a review cannot change the original human or agent attribution.

The original caller signs the exact Thread operation, including account/agent,
Spool, Thread, field, parents, command UUID, value, and a digest-bound original
authority envelope. The public `ThreadControlAuthority` contract carries sealed
Biscuit evidence plus independently verifiable account history and mint-root
attachment. Carrying that evidence never establishes account trust: first
admission checks the independently enrolled account, original publisher, agent,
method, current resource path, actual admission time and typed revocations.
Author-supplied timestamps cannot revive an expired credential.

SQLite writes the successful original-author admission marker in the same
transaction as immutable pending bytes. Missing causal parents can arrive after
that receipt, including after restart. Causal readiness cannot create a missing
admission marker. Exact admitted records retain that proof; current delivery is
still authorized independently. Later hosted review eligibility and landing
recheck current policy and roles.

Interactive writes compare the complete current field frontier atomically.
Historical replication preserves valid concurrent branches. Command UUIDs are
bound to original publisher, Thread and canonical operation, so a retry cannot
change its value. Sharing controls express consent and never grant recipient
resource authority.

`thread_api::thread_control::PreparedControl::sign` consumes the existing
`ThreadOverview.metadata_frontiers`, typed value, original author evidence and
signer. It returns request-ready methods for all five controls. Incomplete or
mismatched frontier versions are rejected before signing. A new review UUID uses
its canonical empty frontier; missing singleton fields require observation.
The caller retains the same prepared command for retries. The example's intent
edit needs discovery, observation and mutation only.

`thread_control_v1.json` contains five Rust-produced canonical fixtures with
complete operation bytes, signatures, IDs, and empty/populated property versions.
The API TypeScript implementation checks the same fixtures. UUIDs use MessagePack
binary values; content hashes and `Vec<u8>` use their existing canonical array
encoding. Clients use the supplied codecs rather than assembling opaque records.

This checkpoint supplies the model, durable admission, native replication facet,
and client preparation. The direct Thread RPC handlers and bounded Thread view
projection are subsequent integration work; the independent daemon method
inventory remains failing until every advertised device method is implemented.

## Verification

On API `2669e52a5abc4724b4ab0198342de88bdb468ee2`:

- `cargo test --locked --offline -p heddle-repo --lib thread_replication -- --nocapture`: 18 passed. Includes real SQLite reordered/concurrent delivery, unrelated fields, stale-CAS rollback, exact retries, restart, missing admission marker and five canonical fixtures.
- `cargo test --locked --offline -p heddle-thread-api --lib -- --nocapture`: 45 passed. Includes prepared commands, bounded native sync, idle no-work, backpressure and cancellation.
- `cargo test --locked --offline -p heddle-thread-api --test thread_workflow -- --nocapture`: 10 passed. Snapshot/edit/update uses three RPCs; local preparation adds no read.
- `cargo test --locked --offline -p heddle-object-model --lib thread_replication::metadata -- --nocapture`: restored 6 passed.
- `cargo test --locked --offline -p heddleco-capability-verifier --lib -- --nocapture`: 22 passed, 1 preexisting ignored, using the generated public authority envelope.
- Model controls independently checked original-authority digest substitution and review actor preservation: removing both conditions failed exactly those two tests, with four unaffected tests passing. Guards restored byte-exact.
- Removing prepared-frontier/version binding made its regression fail because an incomplete parent set produced a signature. Guard restored byte-exact.
- Removing the pending original-authority marker predicate made its regression fail with `Accepted` instead of `Pending`. Guard restored byte-exact.
- Earlier field-isolation and current-frontier CAS omission controls also failed at their intended assertions and were restored.

Real repository and Iroh fixtures require access to their existing local
configuration lock and loopback sockets. A sandboxed run that hit a read-only
configuration lock failed before repository operations; the authorized restored
run above passed with the necessary local access.

The daemon adapter follow-up is verified on API
`104362c71f355c91e0f36c97ad356c28b1ae93ba`:

- `cargo test --locked --offline -p heddle-hosted-client --features client --lib device_rpc::tests -- --nocapture`: restored 1 passed (15.64s). The real no-Weft Iroh workflow now negotiates Metadata, verifies a sealed original-author proof, persists its admission, exports the unchanged original signature, and rejects an unrelated original publisher despite authorized delivery. Existing Checkout/Run/private Source checks remain in this workflow.
- Temporarily removing Metadata from the owned export facets failed specifically during original Metadata export. Restored byte-exact.
- Temporarily omitting original-author verification failed because the delivered operation from a mismatched original publisher was accepted. The test asserts the exact denied operation never persists. Restored byte-exact.
- Current device admission and output checks also honor explicitly revoked publisher keys from independently enrolled authority.
