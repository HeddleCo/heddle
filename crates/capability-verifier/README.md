# heddleco-capability-verifier

`heddleco-capability-verifier` is the canonical, transport-free verifier for
Heddle owner authorization. Weft, Heddle, and browser Worker WASM consumers
can pass the same public evidence for purge or exact timeline acceptance.

The crate is intentionally pure. It does not read a clock, filesystem,
database, environment variable, key store, or network. It does not generate
keys or signatures. Callers supply public evidence, pinned state, and the
evaluation time.

Source moved from `HeddleCo/capability-verifier` at `329c0ae` into the public
Heddle workspace. The standalone line ended at version 0.19 with
`heddle-api = "0.30"`; the Heddle-hosted line resumes at version 0.20 with
`heddle-api = "0.31.0-alpha.1"`. Rust and npm package names remain the same.
This crate has no dependency on Heddle repository, checkout, daemon, or
transport code. The adjacent `heddle-biscuit-verifier` handles ordinary
access; owner authority uses the separate signed operations below.

## Contract

Rust and npm versions follow `[workspace.package] version`; the npm build and
prepack copy the resolved Cargo metadata version into the npm manifest.

Version 0.28.9 consumes `heddle-api = "=0.31.0-alpha.27"`. Public proof types come
from `heddle.api.v1alpha2`; the durable signing formats keep their own versions.
The verifier implements the owner contract:

- `verify_spool_owner_genesis` verifies the owner signature over
  `SHA-256(owner_public_key.public_key || spool_uuid)` and returns the exact
  spool/key binding a caller can TOFU-pin;
- owner roots and every transition recompute their ids and state hashes;
- transition sequences must be gap-free and predecessor-linked, with rotation,
  recovery, policy-change, and deferred-claim signer sets checked exactly;
- a direct capability must carry the singular PURGE action for one exact spool
  selector; capability and Biscuit attenuation cannot grant purge;
- `verify_timeline_acceptance` accepts only one direct format-3
  `ACCEPT_TIMELINE_ORIGIN` capability and its single-block subject Biscuit. It
  checks the v3 signature domain, current pinned owner state, rotation and
  recovery windows, capability and Biscuit revocations, exact origin/Thread
  scope, and the subject signature over the acceptance transcript;
- `canonical_purge_operation` implements the
  `heddle-purge-operation-v2` signing body and binds the leaf subject signature
  to the spool, purge identity, payload digest, and capability id; and
- clone keyrings reverify genesis, root, all accepted transitions, the accepted
  state hash, and any ownership-transfer continuation when loaded.

The signed owner root and complete accepted transition history must prove the
genesis key. Creation can use the current key after rotation, and later rotation
preserves that immutable genesis. The host must compare the creation key with
current account authority atomically with creation. Historical membership alone
does not authorize new creation or capability issuance; issuer retirement and
current purge authority remain independently enforced. A caller constructs its
own local TOFU pin after verifying the evidence.

## Using the verifier

The caller selects its previously pinned genesis evidence and current state
hash; neither may be learned from the authorization bundle being checked.

```rust,no_run
use heddleco_capability_verifier::{
    Decision, PurgeContext, VerificationLimits, verify_purge_authorization_bytes,
};

# fn decide(
#   authorization: &[u8],
#   body: &heddleco_capability_verifier::wire::PurgeOperationSigningBody,
#   payload: &[u8],
#   context: &PurgeContext<'_>,
# ) {
let decision = verify_purge_authorization_bytes(authorization, body, payload, context);
if matches!(decision, Decision::Deny(_)) {
    // Fail closed. Denial categories are stable fixture/telemetry labels.
}
# }
```

### Import publication admission

`import_delegation::verify_commit_preflight` checks a signed Commit against the
host-stored preparation, verified owner context and revocations. Bounded future
starts permit scheduling; Commit grants no genesis admission.

`verify_publication_admission` admits a branch only with its exact P1 genesis
witness and P3 publication: equal witnessed milliseconds and transaction ID,
P1 ordered first, both live inside the single delegation window. It verifies
the signed result and cumulative manifest through the API helper and rechecks
owner, policy and revocations at each observation. Native originals, creator
signatures, envelopes and custody remain independently verified. The complete
receiver uses the API's `verify_import_bundle_witnesses` composition helper.
These admission functions are native Rust APIs; browser signing is preflight.

### Browser and TypeScript consumers

The npm package `@heddleco/capability-verifier-wasm` is generated from this same
crate with `wasm-bindgen`; it is not a second verifier implementation. Build
the publish payload with `npm run build`, call the package's default async
initializer once, then use `verifyPurgeAuthorization` with canonical protobuf
bytes and caller-pinned owner context. The binding also exposes
production transfer, resource-keyring, self/delegated genesis, policy-chain
and import-delegation verification. See the [typed binding API](npm/README.md);
exact generated TypeScript signatures ship in the package.

For timeline acceptance, the caller supplies a previously verified original
endorsement, the actual logical request digest and position range, the exact
Thread's Spool path, the independently pinned current owner state hash, and
current revocation sets. The caller still verifies original credential
provenance and the uploader's independent transport proof.

The Rust crate and npm package versions move together. Publishing is handled by
the release orchestrator after CI has built the WebAssembly and checked the npm
tarball with `npm run pack:binding`.

## Recovery window boundary

`RecoveryPolicy.window_secs` is included in every canonical owner-root and
transition signature. Absence means 604800 seconds. Rotation and deferred
claim cannot change its effective value. A policy transition may change it
only with the current authority signature, the current recovery threshold,
and possession proofs from every next guardian.

Recover must carry a replacement `next_recovery_policy` and possession proofs
in `next_recovery_key_proofs` from every next guardian, using the same policy
validation as a recovery-policy transition. The current guardians authorize
Recover; the new guardians prove possession over the same signed transition.
The new policy may change the window, but activation still waits for the
current policy's window. Recovery installs the next policy and retires all
previous authority issuers in one verified state fold. The old guardian set
must be unable to meet the next threshold, including when only the window
changes. Legacy Recover records that echo the current policy without next
guardian proofs are rejected in native Rust and the WASM/npm package.

The portable transition contains `valid_from_unix_seconds`, but it does not
contain the time at which a recovery or policy change entered pending state.
Callers with that trusted state use `apply_transition_with_timelock` to check
that activation is at least the current window after `pending_since` and to
perform the complete cryptographic and chain verification.
Historical keyring loading can enforce `valid_from` against the supplied clock
but cannot reconstruct a pending-state start or veto. Weft must persist the
hold, accept vetoes, and call the timelock check before committing the entry.

## Limits and conformance

The v2 limits are fixed: a 1,048,576-byte bundle, 256 transitions, 64 capability
entries (only one can authorize purge), 64 grants, 64 path segments of 1–255
UTF-8 bytes, and a 67,108,864-byte raw purge payload. Unknown versions/actions,
non-canonical ids or protobuf, duplicate/gapped/forked history, and oversized
input fail closed.

Portable fixtures under `conformance/fixtures/` cover a valid owner-anchored
purge plus absent evidence, invalid signatures, expiry, wrong spool, wrong
action, attenuation, forged genesis, broken transition chains, transfer
completeness, clone-keyring forks, and format-3 timeline acceptance. The same adapters run natively and under
`wasm32-unknown-unknown` in CI. The differential harness under
`owner-authorization-conformance/` deterministically mutates all four fixture
sets with seeds `38322398`, `1138`, `247`, and `836`, then compares native Rust
with the publishable WebAssembly binding on the identical corpus. It requires
no cross-repository checkout or PAT.

Licensed under either Apache-2.0 or MIT, at your option.

## Production observed evidence

`observed` exposes public native byte APIs for the same alpha.21 protobuf
inputs used by the [production npm bindings](npm/README.md). `policy` owns
complete policy-chain replay, the canonical codec, grow-only revocations,
transfer-aware owner resolution and authority-key exclusion. Part 1's
single-record provenance check reuses this verifier.

The policy implementation and its eight tests were moved from weft's
`crates/weft-authz/src/owner_governance.rs` on integration
(`09c49f6c909bb90e53c9939c4e00bebf215927de`), dropping migration aliases.
**Weft follow-up:** delete that implementation and call this crate's public
`policy::verify_signed_spool_policy_record` / `verify_signed_policy_chain`
APIs (or `verify_resource_policy_chain` with verified ownership). Weft is
unchanged by this PR; it must migrate so there is one policy verifier.

The production differential corpus covers the API's fixed genesis, owner
and signed-policy vectors, existing transfer/keyring conformance, delegated
browser creation, ownership transfer followed by rotation, and explicit
signature, predecessor, handoff-side, sequence-gap, revocation-drop,
self-revocation and backdated-transfer failures. JavaScript dispatches bytes
to public production bindings; native and WASM compare full result objects
and typed error codes. Policy authority stays within the verified entry/exit
handoff states for each ownership phase.
