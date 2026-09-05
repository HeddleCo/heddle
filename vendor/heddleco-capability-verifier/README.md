# heddleco-capability-verifier

`heddleco-capability-verifier` is the canonical, transport-free verifier for
Heddle owner authorization v2. Weft, Heddle, and browser Worker WASM consumers
can pass the same public evidence and get the same purge allow/deny decision.

The crate is intentionally pure. It does not read a clock, filesystem,
database, environment variable, key store, or network. It does not generate
keys or signatures. Callers supply public evidence, pinned state, and the
evaluation time.

## Contract

Version 0.5 consumes `heddle-api = "0.15"` and implements the purge-only v2
contract:

- `verify_spool_owner_genesis` verifies the owner signature over
  `SHA-256(owner_public_key.public_key || spool_uuid)` and returns the exact
  spool/key binding a caller can TOFU-pin;
- owner roots and every transition recompute their ids and state hashes;
- transition sequences must be gap-free and predecessor-linked, with rotation,
  recovery, policy-change, and deferred-claim signer sets checked exactly;
- a direct capability must carry the singular PURGE action for one exact spool
  selector; capability and Biscuit attenuation cannot grant purge;
- `canonical_purge_operation` implements the
  `heddle-purge-operation-v2` signing body and binds the leaf subject signature
  to the spool, purge identity, payload digest, and capability id; and
- clone keyrings reverify genesis, root, all accepted transitions, the accepted
  state hash, and any ownership-transfer continuation when loaded.

There is no service or release key in the spool trust path. The genesis owner
key is the root, and subsequent authority keys are learned only through its
self-verifying transition chain.

## Using the verifier

The caller selects its previously pinned genesis evidence and current state
hash; neither may be learned from the purge sidecar being checked.

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

### Browser and TypeScript consumers

The npm package `@heddleco/capability-verifier-wasm` is generated from this same
crate with `wasm-bindgen`; it is not a second verifier implementation. Build
the publish payload with `npm run build`, call the package's default async
initializer once, then use `verifyPurgeAuthorization` with canonical protobuf
bytes and caller-pinned owner context. The binding also exposes
`verifyOwnerRoot` and the three fixture adapters. Exact generated TypeScript
signatures ship in the package.

The Rust crate and npm package versions move together. Publishing is handled by
the release orchestrator after CI has built the WebAssembly and checked the npm
tarball with `npm run pack:binding`.

## Recovery window boundary

`RecoveryPolicy.window_secs` is included in every canonical owner-root and
transition signature. Absence means 604800 seconds. Rotation, recovery, and
deferred claim cannot change its effective value. A policy transition may
change it only with the current authority signature, the current recovery
threshold, and possession proofs from every next guardian.

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

Portable v2 fixtures under `conformance/fixtures/` cover a valid owner-anchored
purge plus absent evidence, invalid signatures, expiry, wrong spool, wrong
action, attenuation, forged genesis, broken transition chains, transfer
completeness, and clone-keyring forks. The same adapters run natively and under
`wasm32-unknown-unknown` in CI. The differential harness under
`owner-authorization-conformance/` deterministically mutates all three fixture
sets with seeds `38322398`, `1138`, `247`, and `836`, then compares native Rust
with the publishable WebAssembly binding on the identical corpus. It requires
no cross-repository checkout or PAT.

Licensed under either Apache-2.0 or MIT, at your option.
