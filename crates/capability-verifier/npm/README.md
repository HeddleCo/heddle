# `@heddleco/capability-verifier-wasm`

WebAssembly binding for the canonical Rust owner-authorization verifier in
`heddleco-capability-verifier`. Tapestry and other browser consumers execute
the same verifier implementation as native Heddle and Weft callers.

Its version follows the Heddle workspace version. Build and prepack copy the
resolved Cargo version from wasm-pack into this package manifest. If prepack
updates a stale version, it stops packaging so npm cannot use cached metadata;
rerun the pack command with the synchronized manifest.

Build from `crates/capability-verifier` with `npm run build`. The package exports the
standard `wasm-bindgen` initializer plus:

- `verifierVersion()`;
- `verifyOwnerRoot(signedOwnerRootBytes)`;
- `verifyPurgeAuthorization(...)`;
- `verifyTimelineAcceptance(...)`;
- `verifyImportDelegation(...)`;
- `runPurgeFixture(fixtureJson)`;
- `runTransferFixture(fixtureJson)`;
- `runKeyringFixture(fixtureJson)`; and
- `runTimelineFixture(fixtureJson)`.

`verifyPurgeAuthorization` accepts canonical protobuf bytes as `Uint8Array`,
path segments as `string[]`, and both Unix seconds and maximum TTL as `bigint`.
It returns the crate's stable `Decision` serialized as JSON. Call the package's
default async initializer before using any verifier function.

`verifyTimelineAcceptance` takes canonical origin and acceptance protobuf
bytes, the caller's pinned current owner-state hash, exact Spool path, actual
request digest and position range, current capability and subject Biscuit
revocation IDs as hex arrays, admission time, and TTL ceiling. It returns a
boolean. The caller verifies original credential provenance and the uploader's
transport proof independently.

`verifyImportDelegation` takes the original canonical certificate, optional
member permission, clone-keyring inputs and accepted owner history,
independently selected initial owner and Spool genesis, JSON arrays of forbidden
root/witness keys, permanent job associations, cancellations and revoked key IDs,
actual receiver seconds and TTL ceiling. It returns the verified certificate
digest. It verifies current portable permission; historical witness/set/proof
resolution remains in the API trust layer and never uses claimed author time.

The publish root is this `npm/` directory. `npm run pack:binding` from the
crate directory builds it and shows the exact npm tarball payload.
