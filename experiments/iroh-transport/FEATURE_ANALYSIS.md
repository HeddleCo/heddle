# Iroh dependency and feature analysis

The experiment disables Iroh's default features and enables only `tls-ring`:

```toml
iroh = {
  default-features = false,
  features = ["tls-ring"],
}
```

Cargo confirms that the active Iroh feature is only `tls-ring`; the resulting
`noq` features are `runtime-tokio`, `rustls`, and `ring`. This removes Iroh's
optional metrics instrumentation, port mapper, private Apple fast datapath,
platform verifier, qlog, and AWS-LC backend.

It does not remove most of Iroh's dependency weight. On arm64 macOS, the normal
dependency closures in the current workspace are:

| Crate | Packages | Provides |
|---|---:|---|
| `iroh` | 274 | Public-key endpoint identity, address discovery, network watching, hole punching, relays, QUIC |
| `iroh-base` | 95 | Endpoint identity and address types; no transport |
| `noq` | 80 | Socket-addressed QUIC and 0-RTT; no Iroh P2P routing |

The large remaining Iroh closure is structural rather than feature selection.
`iroh-dns`, `iroh-relay`, `netwatch`, `reqwest`, `hickory-resolver`, and the
core `iroh-metrics` crate are unconditional dependencies in this revision.
The `Minimal` runtime preset stops configuring discovery and relay behavior,
but it cannot remove those crates at compile time.

## Public API contract after the v1alpha1 cutover

Current main no longer owns a local `heddle-grpc` schema crate. The published
`heddle-api` 0.1.2 package owns the public protobuf messages and makes tonic's
client/server bindings optional. This experiment enables neither feature:
`cargo tree -p heddle-iroh-transport-experiment --edges normal` contains
`heddle-api` and Prost but no tonic runtime. Its Cargo dependency is named
`api`, and the implementation contains no `grpc` module path.

That lets the transport carry the same typed public messages over Iroh streams
without retaining tonic as a second hosted adapter. The cutover defined by ADR
0049 therefore does not require a simultaneous protobuf-to-postcard rewrite or
a second public contract catalog. Postcard can still be evaluated for private,
Rust-only peer protocols or future CRDT deltas, but cross-product operations
should continue to use the governed v1alpha1 contract and its operation-ID,
signing, retry, cursor, and typed-error rules.

## Framing outcome

One operation now owns one bidirectional QUIC stream. That is a deeper protocol
module than the original split: the caller knows only the operation and typed
messages, while fully-qualified method dispatch, FIN-delimited unary framing,
raw-body lengths, Iroh chunk ownership, receive ceilings, and completion ordering
remain local to the implementation. The method path is the same stable identity
used by the governed contract and request signing, rather than a second numeric
catalog. Independent streams are handled concurrently.

The A/B showed no reason to retain the original tagged control and
connection-global data-stream adapter. Deleting it removed framing complexity
instead of moving it into callers, so it failed the deletion test. A raw-wire
benchmark then isolated endpoint tuning from storage: retaining Iroh's rolling
1.25 MiB stream window, requesting ACK threshold 50, and matching the reusable
receive buffer to the 1 MiB owned send chunks brought the spool adapter within
roughly 1% of raw wire throughput. Larger static windows and all temporary
profile adapters were deleted.

The next performance candidate is overlapping `NativePackStreamingWriter`
flushes with Iroh sends via `GrowingPackChunkReader`. The current prelude
declares total pack and index lengths, so that optimization needs a bounded
chunk/end marker or a separately delimited data stream rather than another
large static window.

## Recommendation

Use Iroh as the hosted and P2P transport defined by ADR 0049. Its extra
dependencies are the implementation of the capabilities Heddle wants:
key-based dialing, discovery, path selection, NAT traversal, browser relay
connectivity, and native relay fallback. Replacing it with `noq` would save
roughly 194 packages in this experiment, but Heddle and Weft would then own:

- certificate issuance or key pinning instead of Iroh endpoint identities;
- peer address discovery and updates;
- NAT traversal and hole punching;
- relay protocol, deployment, path selection, and a separate browser transport.

Do not add a `noq` hosted adapter speculatively. One adapter would create a
hypothetical seam while splitting conformance and operational behavior. Revisit
that choice only if a shipping binary proves Iroh's dependency or runtime cost
material enough to justify a second implementation.

Do not fork Iroh merely to feature-gate its unconditional dependencies until a
real shipping binary proves the 274-package closure is unacceptable. A fork
would turn upstream networking internals into Heddle's maintenance surface.
