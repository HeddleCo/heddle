# HYBRID Part 2 core interfaces

Part 2: `task/1961-hybrid-part2-transport-client`, PR #1963.
Part 1: `task/1961-hybrid-import-authority-host-witness-hed`.

The normative fixed vectors are API alpha.18. Integration's 0.28.7 release now
pins alpha.19; the HYBRID schema, docs, verifiers and vectors are unchanged.
Boundary acceptance stays closed until api#318 / alpha.20. The one
`hybrid::SYNC_MANDATORY_GATE` switch stays OFF until api#307, including Sync
stream openings and RPC preludes.

## Observed public interfaces

All of these are **pending in integration**. Part 2 reads Part 1's branch and
will plain-merge integration when Part 1 lands; it never copies Part 1 files.

- `978eb607`: `capability_verifier::import_delegation::{Selection,
  CurrentContext, VerifiedImportDelegation, Revocation, verify_current,
  verify_historical, verify_historical_genesis, verify_new_operation,
  verify_policy_record}`. Selection is independently verified Spool/owner/keyring
  authority; new work uses receiver time, historical work authenticated witness
  observation/order. Cancellation and key revocations have distinct namespaces.
- `352c99e8`, amended at `b6282ddd`: `crypto::import_authority::{WitnessEvidence,
  NativeClosure, NativeAuthorityContext, verify_native_genesis,
  verify_native_operation, verify_delegated_import, verify_publication,
  verify_genesis_payload, verify_authority_payload, verify_landing_payload}` and
  `object_model::object::thread_replication::delegated_import::DelegatedImport`.
- `b6282ddd`: `repo::thread_replication::hosted_trust::{RootSelection, Clock,
  SystemClock, HostedTrust::open, HostedTrust::mutate, TrustTransaction,
  select_root, replace_root, select_spool}`.
- `b6282ddd`: `delegated_import::{AcceptedAuthority, HostedAdmission,
  NativeSubject, NativeEvidence}` and `ThreadReplica::{install_hybrid_import,
  hybrid_import_bundle, hosted_admission, receive_witnessed}`.

The actual repo installer currently takes:

```rust
ThreadReplica::install_hybrid_import(
    directory: &Path,
    trust: &HostedTrust<impl Clock>,
    bundle_bytes: &[u8],
    native_records: &[wire::SignedRecord],
    authority: &impl AcceptedAuthority,
    store: &impl ObjectStore,
) -> repo::thread_replication::Result<Vec<ThreadReplica>>;
```

`AcceptedAuthority` selects an exact historical `import_delegation::Selection`
for each witness/policy, resolves typed native/import revocations, and checks
**current** disclosure/source access in `authorize_import(bundle, now_millis)`.
`receive_witnessed` resolves `NativeEvidence` under `HostedTrust::mutate`, checks
native original authority and invokes a current disclosure callback. It requires
native dependencies to have been separately admitted already.

## Missing installer callback

Add this final argument to `install_hybrid_import` (or an equivalent public seam
with the same serialization and verification guarantees):

```rust
before_commit: impl FnOnce() -> repo::thread_replication::Result<()>
```

Invoke it only after **all** original/permission/witness/dependency checks,
current disclosure/context/clock checks, and selected replica receives succeed;
keep the shared trust serialization held and the SQL transaction uncommitted.
Any authority rejection must occur before actual repository artifacts mutate.
Part 2 verifies objects in an isolated staging store and installs the actual
pack, native Spool ID and separately selected owner metadata in this callback.
The replica transaction must not commit before source objects are locally
installed. A check followed by releasing the lock and then installing is invalid.
The committed `HostedTrust::mutate` also rechecks receiver time after its closure.
A callback alone is therefore insufficient if that final check can reject after
actual pack/pin writes: the seam must preserve commit-time expiry/rollback guards
and provide transactional filesystem rollback, or an equivalent atomic guarantee.
A late authority rejection must not leave those artifacts installed.

The selected native closure includes ordinary account source, genesis,
ownership claims/resolutions, dependencies and landing evidence when present.
Every requested native original must have its own verified admission or reject
before this callback. Signature verification alone is not admission. Unknown
or uncovered originals fail closed; legacy executor pins cannot authorize them.

## Selected branches and historical prefixes

Fetch selects one source revision. Replication splits native originals into
bounded carriers. Their **complete public bundle** can contain authentic signed
operations/manifests/geneses for other branches or earlier slots whose converted
native counterparts were not requested.

The current installer requires converted native counterparts for every public
bundle operation and installs every branch genesis. That cannot serve these
transports. Verify the complete public delegation/publication/slot history, but
admit only requested native originals and their selected causal dependencies;
never install an unrelated branch from carried public evidence. A per-original
API under `TrustTransaction` is also suitable if it verifies the entire selected
closure before the callback and retains the full unchanged public bundle.
The seam must support an explicitly selected genesis with no converted source
operations, as well as a selected capture and its dependencies.

Historical selections carry the exact transfer prefix at each authenticated
statement. The current `require_public_selection` compares the whole bundle's
final transfer list against that historical prefix. Instead, compare the exact
accepted prefix within the independently selected complete chain. A later
verified handoff cannot erase an earlier authentic original admission.

Part 2's `AcceptedHistory` reconstructs those exact historical contexts from an
independently verified Spool observation. Its eventual `AcceptedAuthority`
adapter must resolve revocations against the exact signed policy and witnessed
accepted order. A generic `false` predicate is forbidden: unknown keys,
credentials, cancellation namespaces or unbound statements reject. Historical
conversion permission cannot supply current disclosure authorization.

## Missing read-only trust snapshot

Async proof-only refresh needs this concrete public accessor:

```rust
pub struct TrustSnapshot {
    pub root: RootSelection,
    pub root_epoch: u64,
    pub previous: Option<api::witness_trust::VerifiedWitnessSet>,
    pub clock_floor_millis: i64,
    pub known_job_associations: Vec<(Vec<u8>, Vec<u8>)>,
}
impl<C: Clock> HostedTrust<C> {
    pub fn snapshot(&self) -> repo::thread_replication::Result<TrustSnapshot>;
}
```

Restore `previous` from the persisted original signed set with its retained
history root, including expired snapshots **only as a high-water/history floor**.
Return the independently selected current root and epoch separately. Reading
never changes the durable clock floor/set; reject detected clock rollback.
Part 2 derives known job keys, performs bounded async set/proof lookup, then
`mutate` reloads and verifies newest trust at use. A set/root/job context learned
while lookup awaits can invalidate the prepared result before installation.

`HostedTrust::mutate` is the sole shared serialization for witness updates and
hosted admission. Freshness, high-water, exact body digest, root epoch, seals,
tombstones, key/job associations and monotonic clock must be rechecked at use.
Root replacement is explicit and preserves authenticated history. Transported
roots or endpoint keys cannot construct independent trust.

## Required concurrent guard-removal evidence

Part 1 owns the production `HostedTrust` guards. Run its concurrent test with
the relevant guard temporarily removed, record the failure, restore the guard,
and record the passing run: stage/resolve N, persist N+1 revocation using an
independent handle, then reject install before durable mutation. Part 2 cannot
edit those guards under the owner's file split. Direct API cached-context tests
do not establish durable install serialization. Part 2 will add the transport
install interleaving test when the actual transaction seam lands.

Part 2 already retains full bundles through staging/batching/relay, rejects
legacy receipt-only authority, retrieves exact bounded retrospective proofs,
refreshes only receiver metadata without replacing originals, and verifies
local conversion/publication/fresh Fetch. Unresolved hosted installation returns
a typed error; no permissive verification placeholder is used.
