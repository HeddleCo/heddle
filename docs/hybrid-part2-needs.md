# HYBRID Part 2 core interfaces

Part 2 branch: `task/1961-hybrid-part2-transport-client`.
Part 1 branch: `task/1961-hybrid-import-authority-host-witness-hed`.
Contract: heddle-api `v0.31.0-alpha.18`; boundary acceptance remains closed
until api#318 / alpha.20. Sync mandatory negotiation remains off until api#307.

These interfaces are pending. Names may follow Part 1's implementation; the
semantics below are required, and Part 2 will consume the actual public APIs.

## Observed Part 1 interfaces (2026-10-03)

Landed on Part 1 branch at `978eb607` (integration pending): `import_delegation::{Selection,
CurrentContext, VerifiedImportDelegation, verify_current, verify_historical,
verify_historical_genesis, verify_new_operation}` and typed `Revocation`.

Landed on Part 1 branch at `352c99e8` (integration pending): `crypto::import_authority::{WitnessEvidence,
NativeClosure, NativeAuthorityContext, verify_native_genesis,
verify_native_operation, verify_delegated_import, verify_publication,
verify_genesis_payload, verify_authority_payload, verify_landing_payload}`;
`object_model::object::thread_replication::delegated_import::DelegatedImport`.
Drafted in Part 1's working tree, not landed:
`repo::thread_replication::hosted_trust::{RootSelection, Clock, SystemClock,
HostedTrust, TrustTransaction, select_root, replace_root}`. Part 2 will use
`HostedTrust::mutate` as the sole witness update/installation serialization.
The newer repo draft also supplies
`delegated_import::{HostedAdmission, NativeSubject, NativeEvidence}` and
`ThreadReplica::{hosted_admission, receive_witnessed}`. `receive_witnessed`
re-resolves trust/policy/originals and calls current disclosure authorization
inside `mutate`; it still requires every native dependency to be separately
admitted. Fetch needs the complete selected closure admitted atomically before
any actual pack or Spool mutation, using the same callback seam below.

## Narrow repo mutation seam still needed

Part 2 retains complete bundles on staged Fetch and on `ReceivedOperation`.
The following concrete API is needed to commit authorized originals using the
**same SQL transaction** as `HostedTrust::mutate`, rather than nesting existing
`ThreadReplica::receive` transactions or checking trust and releasing the lock:

```rust
impl TrustTransaction<'_> {
    pub fn verify_import_source<'a>(
        &self,
        bundle: &'a ImportPublicProofBundleV1,
        selected: import_delegation::Selection<'a>,
        geneses: &'a [ThreadGenesisRecord],
        originals: &'a [SignedOperation],
    ) -> Result<VerifiedImportSource<'a>>;

    pub fn install_import_source(
        &self,
        verified: &VerifiedImportSource<'_>,
        objects: &impl ObjectStore,
    ) -> Result<()>;
}
impl ThreadReplica {
    pub fn import_authority_bundle(
        &self, operation: &ContentHash,
    ) -> Result<Option<ImportPublicProofBundleV1>>;
}
```

`VerifiedImportSource` is opaque, borrowing or owning exact originals/bundle;
verification checks the **entire** closure and every original/dependency before
any install. It is valid only for this transaction (enforce via borrow lifetime
or recheck inside installation). The selected owner/keyring come from local
enrollment or a separate verified Spool observation, never the bundle itself.
Part 2 invokes source pack/Spool/owner installation only after verification,
still inside `HostedTrust::mutate`; Part 1 commits replica frontiers, proof
markers, original authority and bundles atomically in `install_import_source`.
The source verifier must also cover ordinary owned-device originals, claims,
resolutions, genesis/authority/landing witness sidecars mixed with imports.
Unknown or uncovered originals fail closed. Bundle export returns unchanged
original signed public closure plus proof-only history proofs.

For replication, the same seam operates on the carried bundle's complete native
closure and then admits the requested original; no bundle is silently dropped.

Part 1's newer draft exposes `delegated_import::AcceptedAuthority` and
`ThreadReplica::{install_hybrid_import, hybrid_import_bundle}` instead. Those
names work. **One missing hook is critical:** add `before_commit:
impl FnOnce() -> Result<()>` to `install_hybrid_import`, called after every
original/permission/witness check and every `receive_in` succeeds but before the
outer `HostedTrust::mutate` transaction commits. Part 2 passes an isolated staged
`FsStore` for verification and uses the hook to install source objects and native
Spool/owner metadata. This makes rejection leave the actual repository unchanged
and holds the same trust serialization throughout install. No durable pack
installation can occur before full verification; no replica transaction can
commit before the pack is locally installed. Existing `hybrid_import_bundle`
supplies exact retained closure for relay.

Historical selections carry the handoff prefix at each authenticated statement,
not the bundle's final owner/transfer state. The draft's
`require_public_selection` currently compares `bundle.ownership_transfers ==
selection.keyring.wire().ownership_transfers`; this must instead verify the
exact accepted prefix within the complete independently selected chain. A
later verified handoff cannot erase an earlier exact original admission.

The draft also verifies a `NativeClosure` but installs only converted delegated
operations. Every supplied native original must have a verified admission path
or reject before the callback; signature verification alone does not authorize
ordinary account operations, ownership claims/resolutions or dependencies.

Part 2's `AcceptedHistory` reconstructs exact historical owner contexts from an
independently verified Spool observation. Its eventual `AcceptedAuthority`
adapter must resolve typed revocations against the exact signed policy and
authenticated accepted-order witness. A generic `false` predicate is forbidden;
unknown keys, credentials, cancellation namespaces or unbound statements reject.
The draft's new `authorize_import(bundle, now_millis) -> Result<()>` callback
must likewise check independently selected **current** disclosure/source access
under the mutation lock. Historical conversion permission cannot supply it.

Fetch selects one source revision and replication splits originals into bounded
batches. Their complete public bundle can include signed delegated results and
manifests from other branches/earlier slots whose converted native records are
not selected for installation. The draft loops over every bundle operation and
requires a matching converted native record, then installs every branch genesis.
That seam cannot serve a single-branch Fetch or a split replication batch.
Verify the complete public delegation/publication/slot history, but require and
admit converted-native counterparts only for the exact requested originals and
their selected causal dependencies. Never install an unrelated branch merely
because its public proof was carried. Alternatively expose a per-original
verification/installation API under `TrustTransaction`; the public closure must
remain intact through batching, and every requested native original must be
covered or fail closed before the callback.

## Receiver trust and mutation serialization (repo)

The concrete snapshot requested from the newer draft is:

```rust
pub struct TrustSnapshot {
    pub root: RootSelection,
    pub root_epoch: u64,
    pub previous: Option<api::witness_trust::VerifiedWitnessSet>,
    pub clock_floor_millis: i64,
    pub known_job_associations: Vec<(Vec<u8>, Vec<u8>)>,
}
impl<C: Clock> HostedTrust<C> {
    pub fn snapshot(&self) -> Result<TrustSnapshot>;
}
```

Restore `previous` from the persisted original signed set using its retained
history root, including expired snapshots only as a high-water/history floor.
Return the independently selected current root and epoch separately. Do not
claim an expired snapshot is fresh authority. Reading the snapshot never
changes the durable clock floor or set; reject detected clock rollback. Part 2
derives known job keys from the associations, performs bounded async lookup,
then `mutate` reloads and verifies newest trust at use. A set learned during
async lookup can still invalidate the prepared result before installation.

```rust
pub struct HostedRootContext {
    pub authority: String,
    pub root_id: String,
    pub root_public_key: [u8; 32],
    pub root_epoch: u64,
}
pub struct HostedTrustStore; // opened from independently selected local state
impl HostedTrustStore {
    pub fn open(heddle_dir: &Path, root: &HostedRootContext) -> Result<Self>;
    pub fn update_set(&self, signed: &SignedHostedWitnessSetV1, now_ms: i64)
        -> Result<VerifiedWitnessSet>;
    pub fn with_mutation<T>(&self, now_ms: i64,
        mutation: impl FnOnce(&VerifiedWitnessSet) -> Result<T>) -> Result<T>;
}
```

`update_set` and `with_mutation` hold the SAME receiver lock through verification
and commit, persist original set/high-water/digest/seals/tombstones and clock
floor, reject rollback and old-root contexts, and enforce monotonic elapsed time.
`with_mutation` reloads newest state; an earlier opaque verification is never
authority. Root replacement is explicit, invalidates contexts, and carries known
history. A transported root or endpoint key cannot construct independent trust.

## Complete original history verification (crypto / capability-verifier)

```rust
pub struct VerifiedImportHistory; // opaque; complete independently verified closure
pub fn verify_import_history(
    bundle: &ImportPublicProofBundleV1,
    selected_spool: &IndependentlySelectedSpoolLineage,
    set: &VerifiedWitnessSet,
    now_ms: i64,
) -> Result<VerifiedImportHistory>;
```

This verifies owner histories/handoffs/policies, original genesis/envelopes,
owner-to-device typed permissions, each original delegation, converted native
operation and exact publication/admission/authority/landing statements. Historic
authority time comes ONLY from authenticated witness observation/order. Resolve
each issuer and exact proof independently. Missing/extra/substituted closure
rejects. BoundaryAcceptance rejects until api#318. Persist key-to-job associations
under the receiver mutation lock and forbid all root/user/witness role overlap.

## Verified batch installation and relay (repo)

```rust
pub fn install_verified_source(
    repository: &Repository,
    history: &VerifiedImportHistory,
    geneses: &[ThreadGenesisRecord],
    originals: &[SignedOperation],
    pack: &Path,
    index: &Path,
) -> Result<StateId>;
```

The batch must match every native original/dependency to the verified history
BEFORE Spool ID, owner observation, replicas, immutable pack, source frontiers or
possession markers change. Reject leaves no partial authorized mutation. Commit
uses the same receiver lock as trust updates; receipt verification may not fall
back to evergreen executor pins. Local-key and owned-device paths retain their
independent current authority checks. A relay exports the unchanged complete
public bundle attached to stored operations, never just signatures.

Part 2 implements proof-only retrieval using api's exact leaf preimage, bounded
DTOs and inclusion verification, and re-resolves staged originals against newest
trust rather than treating structural staging or cached contexts as authority.

## Required concurrent guard-removal evidence

Part 1 owns the production `HostedTrust` high-water/re-resolution guards.
Please run its cached-context/concurrent-install test with the relevant guard
temporarily removed, record the failing run, restore it, and record the passing
run. Part 2 cannot edit that production code under the owner's file split.
Direct API context tests are useful controls but do not prove durable install
serialization. Part 2 will add the transport install interleaving test after the
actual transaction seam lands in integration.
