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

The daemon now routes StartThread, RenameThread, ReviseIntent, ChangeLifecycle,
SetSharingPolicy, RecordReview and ObserveThread to real local operations. Name
and lifecycle compare their own portable field versions, just like intent,
sharing and each review UUID; edits to other fields do not invalidate them.
Command journals bind delivery actor, method and exact request, while an exact
already-admitted operation also survives loss of the response journal.

Thread observation composes overview, source captures, original review decisions,
exact source/base comparisons and sharing policy. Optional original operations
retain their signatures. Review diff/evidence composition is still partial;
collaboration, analysis, checkouts and timeline composition remain explicitly
unavailable in this adapter until their shared read handlers are connected.
The independent daemon method inventory remains failing until every advertised
device method is implemented; route registration is not a completeness claim.

The source count and frontier are durable SQLite indexes changed exactly once
at source acceptance, in the same transaction. Views read current bounded field
heads rather than operation history. Filesystem events wake observers, but the
actual committed Thread revision fences output and checkpoints. Duplicate OS
hints do not invalidate a stable snapshot. Unchanged idle streams perform local
clock/caveat checks and no periodic storage reads.

Interactive owner/delegate sharing enables publication consent for that Thread
once. Later admitted policies from that same account govern ongoing native
Source, Collaboration and Metadata sync without another consent action. A
conflicting policy or foreign original actor cannot broaden private device
consent. Evidence and scrubbed timeline transport remain separate unfinished
facet integration work. Merely receiving a policy cannot establish initial
local publication consent.

Ordinary Fetch staging already selects Source alone. Installation also checks
Source explicitly before accepting any operation: a source verifier callback
cannot establish Metadata original-author admission. Shared foreign-account
metadata still needs portable hosted admission evidence bound to its exact
original operation; the account envelope cannot establish its own trust.

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


## Direct Thread acceptance checkpoint

Against API `104362c71f355c91e0f36c97ad356c28b1ae93ba`:

- Real no-Weft device RPC workflow: 1 passed (21.86s), covering all seven Thread methods alongside existing Checkout, Run and bidirectional replication checks. It proves independent field updates, stale same-field rejection, original creator rejection, exact retry after response-journal loss, signed review push and idle no-heartbeat behavior.
- Restored native repository suite: 21 passed (6.88s), including durable source count/frontier, out-of-order acceptance, exact replay, concurrent heads, rollback and one-time local policy consent.
- Source-index controls removed count/bound maintenance and parent-head deletion; all failed at the intended assertions and were restored byte-exact.
- Device controls independently removed current-field CAS and original creator binding; both failed at their real RPC assertions. Publication actor and shared signature-key binding omission controls also failed at the precise retained-consent and framing assertions. All guards restored byte-exact.
- Rust Name/Lifecycle builder control replacing the field version with the whole Thread version failed; restored builder suite passed. Shared verifier rejects unknown formats, extra signatures and a substituted publisher key field.
- Explicit source installation guard: 1 passed (0.38s), with a valid signed Metadata fixture proving denial before durable bytes or original-author admission marker. Public staging was already Source-only; this strengthens the installation boundary rather than claiming a previously reachable public bypass.

Final restored checks on API `988e4bbf0a66f6741a4bfc81e104dbfe918d1a9f`:

- Native repository Thread suite: 22 passed (6.92s).
- Full shared Thread API library suite: 51 passed (14.96s), including source installation, portable controls, evidence codecs and native stream backpressure/cancellation.
- Production source page query executes 148 VM instructions for a 16-row first page with 32 Source records, 148 with 10,032 Source plus 10,032 unrelated-facet records, and 147 for a deep cursor. Its covering index and range predicate keep work proportional to the requested page.
- Removing the covering index caused 704 instructions at the small fixture; restoring the nullable cursor `OR` caused 60,190 instructions at the deep page. Both exceeded the asserted bound and failed; both restored byte-exact.
- Removing the source-only installation guard admitted the valid signed but unproved Metadata fixture and failed its intended assertion. Guard restored; the complete 51-test client run above includes the final positive.
- Final restored real no-Weft device RPC workflow on API988: 1 passed (21.48s), retaining all Thread, Checkout, Run, original-author replication and idle checks after every control was restored.

### Retained stream admission

The native router admits at most 32 unfinished handshakes/short commands. After
local authentication succeeds, each server or bidirectional stream exchanges
that slot for one of 2,048 retained-stream slots. Incoming method names alone
cannot trigger promotion. Cancellation, rejected promotion and completed streams
release their owned permits; no background quota replenishment is needed.

The real Iroh regression holds 40 Thread views while it executes and verifies a
durable StartThread, then closes all views. It measures all 32 admission slots,
all 2,048 retained slots, and the shared filesystem feed before and after each of
two cycles. Removing the admission release fails exactly at the 33rd view. These
checks establish capacity and cleanup behavior, not a latency benchmark.
