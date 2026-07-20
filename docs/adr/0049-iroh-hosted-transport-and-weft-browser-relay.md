---
status: accepted
---

# Iroh hosted transport and Weft browser relay

Heddle, Weft, and Tapestry will use Iroh as the transport for the governed
`heddle.api.v1alpha1` hosted call surface and will remove tonic from the product
network stack through a coordinated hard cutover. The protobuf contract remains
owned by `HeddleCo/api` and Prost remains its Rust message codec; changing the
transport does not change Heddle's durable encodings or require a simultaneous
postcard migration.

The hosted call module exposes unary, server-streaming, and bidirectional
streaming operations behind one interface. Its Iroh implementation uses one
bidirectional QUIC stream per logical operation, fully-qualified contract method
paths for dispatch and signing, FIN-delimited unary bodies, length-delimited
message streams, and explicitly framed raw phases for packs and indexes. Generated
method descriptors and a generated router replace tonic's generated client and
server interfaces. Callers do not own ALPN negotiation, framing, retry, stream
ordering, or transport error conversion.

Weft owns the Iroh relay deployment used by Heddle clients and the Weft Iroh
application endpoint that terminates hosted calls. They are separate modules and
independently scalable deployables: the relay provides connectivity and forwards
encrypted Iroh traffic, while the application endpoint terminates QUIC, dispatches
contract methods, and enforces hosted identity, policy, quotas, and repository
authorization. Co-locating them in one release or region does not merge their
interfaces or trust responsibilities.

Native clients try direct UDP paths and fall back to a Weft relay. Browser clients
run Iroh in WebAssembly and reach a Weft relay over secure WebSocket because
browsers cannot expose Iroh's native UDP transport. This is not browser
WebTransport, and the first cut does not add a second HTTP business-call fallback.
Both paths carry the same ALPN, method descriptors, protobuf messages, call
context, failure envelope, and collaboration streams to the same application
endpoint.

The first production protocol advertises ALPN `heddle-api/1`. The earlier
`heddle-sync/3` value remains experiment-only and is not accepted by production
clients or endpoints.

Connection bootstrap uses a signed HTTPS endpoint descriptor containing the Weft
endpoint identity, relay URLs, supported ALPN versions, optional direct addresses,
and expiry and rotation data. A browser persists a distinct Iroh device key in
IndexedDB; it does not reuse a Biscuit proof-of-possession or human-signing key as
its transport identity. Relay admission uses a short-lived bootstrap token bound
to the authenticated subject and browser endpoint identity. The application call
context carries deadlines, bearer capabilities, proof-of-possession, human
signatures, client operation IDs, and trace context as typed fields rather than
tonic metadata.

Only descriptor-declared `READ_ONLY` operations may use QUIC 0-RTT initially.
Transient and durable writes remain on replay-safe 1-RTT paths until their
idempotency and replay behavior has cross-product conformance coverage. Transport
failures use a contract-owned failure envelope with stable code, message, and
typed details rather than `tonic::Status` or gRPC-specific fields.

## Consequences

- The production cutover ports the shipped contract surface, not every planned
  RPC in the shared catalog, and does not retain a permanent tonic adapter or
  dual-protocol serving period.
- The current Iroh experiment is promoted by first proving `ListRefs`, one
  authenticated read, and one idempotent write against a Weft endpoint; remaining
  unary calls follow before live collaboration, `Push`, and `Pull` streaming.
- Browser and native conformance tests exercise the same endpoint and exact
  method-signing fixtures. Relay tests separately cover admission, origin policy,
  quotas, idle limits, reconnects, draining, and regional failover.
- The unused local gRPC daemon interface is deleted rather than ported to Iroh.
  Future local process communication needs a separately justified interface.
- Product crates remove tonic, tonic-health, and gRPC-specific naming after the
  coordinated cutover. Telemetry uses a transport that does not reintroduce tonic
  as a product networking dependency.

## Considered options

Keeping tonic for hosted unary calls and Iroh only for packs or peer traffic
would leave authentication, errors, retries, observability, and generated routing
split across two shallow transport interfaces. Using `noq` for the fixed hosted
path would reduce dependency weight but would make endpoint identity, relay
selection, browser connectivity, NAT traversal, and future peer routing Heddle
and Weft implementation concerns. Using browser WebTransport would create a
second transport implementation instead of exercising Iroh's relay path. One
Iroh call interface preserves locality while keeping the public protobuf contract
independent of its transport.
