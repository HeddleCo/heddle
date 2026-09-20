# `@heddleco/capability-verifier-wasm`

WebAssembly binding for the canonical Rust owner-authorization verifier in
`heddleco-capability-verifier`. Tapestry and other browser consumers execute
the same verifier implementation as native Heddle and Weft callers.

Build from the repository root with `npm run build`. The package exports the
standard `wasm-bindgen` initializer plus:

- `verifierVersion()`;
- `verifyOwnerRoot(signedOwnerRootBytes)`;
- `verifyPurgeAuthorization(...)`;
- `runPurgeFixture(fixtureJson)`;
- `runTransferFixture(fixtureJson)`; and
- `runKeyringFixture(fixtureJson)`.

`verifyPurgeAuthorization` accepts canonical protobuf bytes as `Uint8Array`,
path segments as `string[]`, and both Unix seconds and maximum TTL as `bigint`.
It returns the crate's stable `Decision` serialized as JSON. Call the package's
default async initializer before using any verifier function.

The publish root is this `npm/` directory. `npm run pack:binding` from the
repository root builds it and shows the exact npm tarball payload.
