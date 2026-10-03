# HYBRID Part 2 core interfaces

Part 2 branch: `task/1961-hybrid-part2-transport-client`.
Part 1 branch: `task/1961-hybrid-import-authority-host-witness-hed`.
Contract: heddle-api `v0.31.0-alpha.18`; boundary acceptance remains closed
until api#318 / alpha.20. Sync mandatory negotiation remains off until api#307.

These interfaces are pending. Names may follow Part 1's implementation; the
semantics below are required, and Part 2 will consume the actual public APIs.

## Observed Part 1 interfaces (2026-10-03)

Landed on Part 1 branch at `978eb607`: `import_delegation::{Selection,
CurrentContext, VerifiedImportDelegation, verify_current, verify_historical,
verify_historical_genesis, verify_new_operation}` and typed `Revocation`.

Drafted in Part 1's working tree: `crypto::import_authority::{WitnessEvidence,
NativeClosure, NativeAuthorityContext, verify_native_genesis,
verify_native_operation, verify_delegated_import, verify_publication,
verify_genesis_payload, verify_authority_payload, verify_landing_payload}`;
`object_model::object::thread_replication::delegated_import::DelegatedImport`;
`repo::thread_replication::hosted_trust::{RootSelection, Clock, SystemClock,
HostedTrust, TrustTransaction, select_root, replace_root}`. Part 2 will use
`HostedTrust::mutate` as the sole witness update/installation serialization.

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

## Receiver trust and mutation serialization (repo)

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
