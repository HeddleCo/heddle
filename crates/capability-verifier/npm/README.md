# `@heddleco/capability-verifier-wasm`

Production browser bindings for `heddleco-capability-verifier`. All proof
verification runs in Rust. This revision builds version **0.28.10**, using
`heddle-api` **0.31.0-alpha.37**. Tapestry should pin
`@heddleco/capability-verifier-wasm@0.28.10` exactly when that artifact is
published. If the next Heddle release bumps the workspace version, pin that
exact release version instead: CI requires npm, Cargo and the validated tag
to agree. This PR does not publish a package.

Configure `@heddleco:registry=https://npm.pkg.github.com` in the consumer's
`.npmrc`, with its usual GitHub Packages read credential.

```ts
import init, { verifyResourceKeyring, verifySignedPolicyChain } from
  "@heddleco/capability-verifier-wasm";
await init();
const lineage = verifyResourceKeyring(keyringBytes, ownerStateBytes, nowSeconds, ttlSeconds);
const policy = verifySignedPolicyChain(policyRecords, keyringBytes, ownerStateBytes, nowSeconds, ttlSeconds);
```

Protobuf inputs are `Uint8Array` containing the matching alpha.37 message
bytes, with canonical protobuf encoding. Unknown fields, repeated-field
aliases, wrong widths and oversized evidence fail closed. There is no TS
verification implementation. Encode with the matching `@heddleco/api` schema
if the transport gives decoded messages. All time and sequence arguments are
`bigint`; result sequences are lossless decimal strings and byte values are
lowercase hex. Every production rejection throws a `VerificationError`
object with an exhaustive `code` union and a diagnostic `message`; dispatch
on `code`. Successful results are JS objects, not JSON strings.

Each integer is checked before conversion: sequences/positions must fit `u64`,
and times/TTL must fit `i64`. Numbers and other JS types throw a typed `invalid`
error. Native time and positive-TTL rules still apply; import seconds-to-ms
conversion rejects overflow. Policy revocations are capped at 4096 per record
and cannot introduce any authority key from the signing owner's verified
accepted history. Owner histories use the general 256-transition bound.

| Binding | Signature | Observed wire messages / result |
| --- | --- | --- |
| `verifyOwnerRoot` | `(root: Uint8Array): OwnerSummary` | `SignedOwnerRoot`; exact root authority |
| `verifyOwnershipTransfer` | `(transfer, sourceHistory, destinationHistory, resourceUuid: Uint8Array, expectedSequence, nowSeconds, ttlSeconds: bigint): TransferSummary` | `ResourceOwnershipTransfer` and both exact `OwnerHistory` witnesses; verifies source handoff and destination acceptance |
| `verifyOwnershipTransferChain` | `(keyring, ownerState: Uint8Array, nowSeconds, ttlSeconds: bigint): ResourceKeyringSummary` | Full accepted audits and both sides of every handoff |
| `verifyResourceKeyring` | `(keyring, ownerState: Uint8Array, nowSeconds, ttlSeconds: bigint): ResourceKeyringSummary` | `ObserveOwnership.owner.resource_keyring` (`CloneAuthorizationKeyring`) and the complete observed `OwnerState` |
| `verifySpoolOwnerGenesis` | `(genesis: Uint8Array, nowSeconds: bigint): GenesisSummary` | `SignedSpoolOwnerGenesis`, self-signed or with `SpoolCreationProof` |
| `verifySignedPolicyChain` | `(records: Uint8Array[], keyring, ownerState: Uint8Array, nowSeconds, ttlSeconds: bigint): PolicySummary` | Complete ordered `SpoolEvent.signed_policy` records from the empty head, with transfer-aware authority |
| `verifyImportDelegation` | `(certificate, permission, keyring, acceptedOwnerHistory, selectedInitialOwnerId, selectedSpoolGenesisDigest: Uint8Array, forbiddenKeysJson, jobAssociationsJson, cancellationsJson, revokedKeysJson: string, nowSeconds, ttlSeconds: bigint): ImportSummary` | Original import certificate and optional member permission; independently selected lineage, current portable permission and certificate digest |
| `verifyNativeGenesisAuthority` | `(binding, original, envelope, keyring, currentOwner, authorHistory: Uint8Array, admittedMintRootsJson: string, selectedInitialOwnerId, selectedSpoolGenesisDigest: Uint8Array, revokedKeyIdsJson, revokedCredentialIdsJson: string, nowSeconds, ttlSeconds: bigint): NativeGenesisSummary` | Exact creator binding and selected lineage; account checks StartThread authority, LocalKey still requires a separate hosted ownership claim |
| `verifyPurgeAuthorization` | `(authorization, operationBody, payload, genesis, currentStateHash, spoolUuid: Uint8Array, path: string[], nowSeconds, ttlSeconds: bigint): PurgeDecision` | Typed Rust purge decision |
| `verifyTimelineAcceptance` | `(origin, acceptance, acceptedStateHash: Uint8Array, path: string[], requestSha256: Uint8Array, firstPosition: bigint, eventCount: number, revokedCapabilityIds, revokedSubjectIds: string[], nowSeconds, ttlSeconds: bigint): boolean` | Format-3 acceptance; false denies invalid evidence |
| `verifierVersion` | `(): string` | Exact Cargo/npm version |

`ResourceKeyringSummary` includes the sole current owner, its exact accepted
state hash (including rotations after a transfer), the accepted transfer
sequence, ordered handoffs and audit hashes, and the immutable genesis digest.
`PolicySummary` includes the verified policy sequence/hash, signing owner and
state, transfer sequence, grow-only revoked key ids and optional audience.
The current OwnerState is required because transfer witnesses stop at the
handoff and cannot describe later rotations.

Proof integrity does not independently select trust. Retain and compare your
own initial-owner/genesis pins and accepted policy head. Historical genesis
verification is not fresh creation admission or a host permission check.
Import delegation verifies current portable permission; historical hosted
witness/set/proof resolution remains in the API trust layer.

Native Genesis resolves the creator's own `authorHistory` independently of the
Spool's `currentOwner`. `admittedMintRootsJson` supplies authenticated attachment
admission: hosts use durable enrollment, receivers use verified witness evidence.
A verified issuer can survive Rotate; Recover cuts attachments from earlier
issuers. Incoming certificates alone cannot enroll an attachment.

The `run*Fixture` exports are conformance harnesses, not production APIs.

Build with Rust's wasm32-unknown-unknown target and `wasm-bindgen-cli 0.2.127`:

```sh
cargo install wasm-bindgen-cli --version 0.2.127 --locked
rustup target add wasm32-unknown-unknown
npm run build
npm run pack:binding
```

Run those npm commands from `crates/capability-verifier`. Build uses Cargo's
locked graph, generates web bindings and TypeScript declarations, includes
both licenses, and synchronizes the npm version from Cargo metadata.
`npm/dist` is ignored and built in CI. Prepack refuses stale npm-cached
version metadata; synchronize with `npm run build` before packing.

Heddle's release workflow builds and packs from the validated tag SHA, checks
that npm/Cargo/tag versions match, then publishes the tarball to GitHub
Packages after the stable binary release succeeds. The publish job uses the
approval-protected release environment and a step-scoped GITHUB_TOKEN.
Prerelease/manual dry runs build the artifact without publishing it.

`NativeGenesisSummary` is a discriminated union. `owner_kind: "account"` reports
verified StartThread authority with `requires_hosting_claim: false`.
`owner_kind: "local_key"` reports only a verified creator binding with
`requires_hosting_claim: true`; callers must separately verify the hosting claim
and source cutoff before granting hosted authority.

HYBRID uses the alpha.33 scope: one positive `max_result_bytes` total per logical
job plus `max_operations`; branches carry no byte allowances. `remainingImportScope`
accepts the original signed scope and cumulative committed manifest as exact
protobuf bytes. Every consumed slot must belong to that scope; it subtracts the
consumed total and operations once. Reapplying an old manifest to a remainder
refuses. The `u64` total stays lossless in encoded bytes.
Clocks and TTLs continue to require checked `bigint` inputs. Retries
cannot reset budgets; sibling jobs have separate signed totals.
