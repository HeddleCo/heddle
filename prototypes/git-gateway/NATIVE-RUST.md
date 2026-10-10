> Preserved historical immutable read/cache prototype. Current Heddle 0.30.0 hosted authentication, receiver acceptance and publication code is described in [HOSTED-PUBLICATION.md](HOSTED-PUBLICATION.md) and [HOSTED-RUNTIME.md](HOSTED-RUNTIME.md). Statements below about missing hosted interfaces apply to this earlier path, not the current implementation.

# Native Rust gateway and visibility boundary

The Cloudflare candidate now runs one Rust executable, `gateway_host`. The small
JavaScript Worker still owns the Artifacts/R2/Container bindings. Python remains
in historical reference implementations and local test/fixture tooling, not in
the Cloudflare serving image or its request path.

## Request path

1. The Worker checks its reader/service policy and the pinned Artifacts manifest.
2. The Rust ingress validates the exact route, Host, framing and both bearers.
3. It rereads its bounded configuration, reads the exact manifest through the
   fixed private bridge, and requests a current disclosure decision.
4. A cache miss reads the SHA-256-pinned R2 native bundle. A bounded child of the
   **same Rust binary** extracts private metadata and directly calls Heddle's
   `export_public_native_view_with_authorized_threads`. Keeping this cold work in
   a child preserves hard memory/CPU/file limits and a killable deadline; it is
   not a second HTTP service or a Python wrapper. The child validates exact OID
   and strict Git fsck before admission to the disposable cache.
5. Each request copies only its exact validated closure into a private Git sink.
   Bounded `git-http-backend` supplies ordinary smart-HTTP Git transport.
6. Rust rechecks reader/service configuration and disclosure generation before
   sending any Git bytes. Validated output is copied from a temporary file to
   the socket in bounded chunks, with no whole-response RAM copy or second proxy.

This is native Linux Rust. Rust Workers use a WebAssembly runtime; directly
porting the present Heddle filesystem/SQLite/projection/process stack there is
separate work. This implementation reuses native Heddle code without asserting
that it already runs unchanged as a Worker WASM module.

## Current visibility, not just a frozen bundle

**Hosted serving is deliberately unavailable until canonical current Heddle
visibility authority is connected.** The existing Cloudflare private bridge
returns 503 for `/disclosure/<pin>`; static gateway policy cannot mint a receipt.
The Worker activation gate also remains disabled. Removing Python does not
remove these integration gates.

A native host requires canonical, bounded disclosure JSON from the fixed bridge
before a cache hit/miss and again after Git response construction. The contract
binds `schema`, `authority`, `allow`, `audience`, `reader_sha256`, `pin`,
`manifest_sha256`, `native_sha256`, `threads_sha256`, `generation`, and
`expires_at`. The digest fields bind canonical manifest bytes and a sorted
canonical Thread list. The decision must expire within 30 seconds and no later
than its configured reader/service grant; both decisions must preserve identical
scope and generation. Missing, malformed, duplicated, expired or mismatched
receipts fail closed.

A future `heddle-current-disclosure` adapter must authenticate current native
state/entry visibility, original-author restrictions, dependency Thread audience,
admission/ownership, redaction and retention for the complete selected closure.
It must bind those facts and the immutable bundle to the same generation. This
contract is not itself such an authority implementation. The one-reader MVP
binds the receipt to the authenticated configured reader; multi-reader expansion
requires explicit reader-bound authority requests, not caller-selected audience.

The exporter currently permits only complete **Public** native closures. It
conservatively rejects nonpublic state or entry visibility anywhere in ancestry,
even for snapshot projection; it does not emit per-reader filtered private
views. Original signed capture restrictions also remain binding. Redacted bytes
are replaced with their native redaction stub. An elapsed embargo does not
silently promote visibility. All admission, original signatures, explicit
same-local-key Thread dependencies and existing resource limits remain checked.

Checks authorize a point in time. They cannot revoke bytes already cloned or
promise that a later mid-delivery revocation erases bytes already sent.

## Cache model

Only successful immutable projections are cached, packed once after validation
to reduce repeated file hashing and copying. Keys bind projection format,
full manifest (including epoch/mode/state), source bundle digest, every explicit
Thread grant, reader, audience, authority kind and current disclosure generation.
Each cache entry has its own bare object database. Objects from different views
are never mixed and no Git alternates are used. A bounded fingerprint checks
cached bytes before copying; corruption fails closed rather than serving them.

The process-private cache holds at most four entries / 128 MiB, with a 96 MiB
per-projection ceiling. Entries become ineligible for reuse after 60 seconds
and are lazily discarded at the next authorized materialization or shutdown. It keeps derived Git objects, not native
repositories or signing identity. Eviction/restart loses everything safely;
`--no-cache` exercises native reconstruction each time. This is not a full Git
mirror. Reader/service authorization, catalog validation and fresh disclosure
checks are never cached. A new view, bundle, disclosure generation, epoch or
Thread grant cannot borrow an old hit.

## Local verification

Build the examples and run native tests:

```sh
cargo build --locked -p heddle-git-projection --examples
cargo test --locked -p heddle-git-projection --example gateway_host
cargo test --locked -p heddle-git-projection --test gateway_visibility_security
export GATEWAY_NATIVE="$PWD/target/debug/examples/gateway_native"
export GATEWAY_HOST="$PWD/target/debug/examples/gateway_host"
PYTHONPATH=prototypes/git-gateway python3 -m unittest discover \
  -s prototypes/git-gateway -p test_rust_host.py -v
```

The test driver supplies canonical configuration plus catalog/native/disclosure
responses on loopback. `--local-test --config FILE --bind 127.0.0.1:PORT --bridge
http://127.0.0.1:PORT` accepts only explicitly labeled `quiescent-synthetic`
disclosure fixtures, not hosted authority. It is a test seam, not a mode that
allows hosted deployment to ignore current state visibility. Production fixes
the ingress/bridge origins and accepts only `heddle-current-disclosure` receipts.

The runtime image needs Git and the Rust binary, without Python. Docker image
execution and actual Cloudflare deployment remain unverified until separately
approved and tested. Matched benchmark results are recorded in the deliverable;
debug/release, cold/warm, direct/Worker and tiny/medium cases must remain distinct.

## Local write/full-history milestone

The separate [local Git window](GIT-WINDOW.md) adds an explicitly loopback-only
receive path with durable native acceptance and recoverable local publication.
The immutable cached read path and its saved performance evidence are unchanged.
See that document for supported scope and the remaining hosted integration gates.
