# Native Heddle ↔ Git gateway

Current hosted implementation on Heddle 0.30.0: [publication and recovery](HOSTED-PUBLICATION.md)
and [concrete Rust runtime, configuration and diagnostics](HOSTED-RUNTIME.md).
It adds real Biscuit proof-of-possession sessions, receiver-owned native/Git
acceptance, complete supported history and recoverable R2/Artifacts publication.
Cloud activation and enrollment remain separately gated. Consult the packaged
verification report for executed integration results; source wiring alone is not proof.

The preserved [immutable read gateway](NATIVE-RUST.md) and [local Git window](GIT-WINDOW.md)
remain independently runnable reference paths. The sections below document that
older read-only/loopback prototype, including its historical Python test adapters
and frozen bundle format. They are not the current hosted architecture or its
remaining-work list. [CURRENT-RUN.md](CURRENT-RUN.md) preserves the earlier
0.29.0 format-v6 demo checkpoint, and [GIT-HISTORY.md](GIT-HISTORY.md) covers the
shared bounded full-history projection API.

Cloud Linux host/container preparation and its explicitly local verification are
documented in [HOSTING.md](HOSTING.md). The authenticated host is a separate
entrypoint; the fixture server below retains its local-only semantics.

This prototype serves ordinary Git clone/fetch from **Heddle-native objects**. A separate,
small Git catalog records immutable published-view manifests suitable for Cloudflare
Artifacts. A catalog commit ID identifies metadata; `git_oid` identifies the projected
source commit. They are deliberately different. Git clients use `/views/<catalog-commit>.git`
at the gateway. A pointer blob is **not** a Git object indirection protocol.

All source is Apache-2.0 under the repository's [LICENSE](../../LICENSE). Nothing here has
been deployed, pushed to an external remote, or tested against a live Artifacts/R2 account.
No real customer content is used. The local demo creates Heddle's normal synthetic offline
signing identities inside its own output directory; it does not enroll a device, mint a hosted
credential, invent a signature, or use the operator's identity. Do not deploy this demo server.

## Architecture and implemented boundaries

```
Git client → Worker front door → native gateway → fresh, bounded bare Git repo
                 │                    │                   │
       Artifacts catalog pin    authorized native read   git-http-backend
        (manifest Git repo)     (fixture .heddle store)      upload-pack
```

* `crates/git-projection/src/gateway_view.rs` is the original read-only public Rust boundary.
  It reuses `export_state`, `export_tree` and `SyncMapping`; it does not rewrite storage.
  It checks native Thread admission, verifies original operation signatures, intersects
  original captured privacy and current sidecars, and rejects unresolved ancestry.
  All ancestors are checked even for snapshot output. Only an empty, parentless root
  named by verified Thread genesis may lack a source operation. It fails closed on
  imported/fidelity states in the original view mode, withheld entries, gitlinks
  and child-spool edges. The separate full-history API accepts signed, admitted,
  byte-faithful imported states and rejects redacted history.
* `crates/git-projection/src/gateway_write.rs` implements bounded, fast-forward
  native acceptance with exact Git identity, current writer checks, signed
  originals and atomic retry receipts. `examples/gateway_host/window.rs` adds
  the explicit local stable URL, isolated quarantine and recoverable publication.
* `examples/gateway_native.rs` creates real synthetic native Threads using the current
  `create_native_thread`, `materialize_thread`, and `capture_thread_from_disk` paths.
  Two concurrent worker threads write separate checkouts, synchronized at a barrier;
  Heddle serializes admission where required. This is concurrent synthetic agent work,
  **not** a demonstration of two remote LLM agents or hosted Weft agents.
* `gateway/core.py`: a local Git catalog with canonical JSON, atomic ref compare-and-swap,
  exact-parent idempotent replay/lost-ack recovery and published-lineage pin validation;
  per-request fixture scope checks; configured logical source resolution; fresh projection.
* `gateway/artifacts.py`: a real read-only REST implementation of the same `resolve(pin)`
  catalog interface. It reads the documented raw `/file?ref=<commit>&path=manifest.json`
  response with an existing caller-supplied credential, fixed HTTPS API host, disabled
  redirects/proxies and bounded reads. Tests inject documented-response fixtures; no live
  request has been made. It is programmatically substitutable in `server()`; the CLI stays
  deliberately local-only pending production authentication.
* `worker/catalog.mjs`: the matching binding adapter. Python and JavaScript use the same
  canonical manifest fixtures, reject duplicate/unknown fields and unsafe epochs, and
  require exact commit pins in a trusted publication list.
* `gateway/http.py`: loopback-only smart HTTP GET discovery and POST upload-pack through
  native `git http-backend`. No receive-pack, dumb object routes, arbitrary filesystem
  paths, native storage-pack forwarding, persistent object cache, or write protocol.
  Each request gets a fresh private bare repository containing only the selected closure.
* `worker/index.mjs`: Worker front door and real Artifacts **read-interface implementation**.
  It calls `ARTIFACTS.get()` and `repo.readFile({ref: pin, path: 'manifest.json'})`, then
  disposes the capability. Authorization precedes catalog reads. Forwarded headers are
  allowlisted; fixture identity headers are stripped. Its tests use explicit test doubles.
  The fixture server also rejects Authorization/Origin/proxy headers and non-loopback Host
  values, so pointing the Worker at that server does not create production authentication.

The reference adapter copies bounded native metadata into a request-private directory
before opening Heddle, because normal repository open may reconcile local indexes. The
copy excludes the offline signing key. Source immutability is checked after the E2E suite.
Whole native packs may exist **inside that private source copy**, but they are never
installed in the Git sink or returned to clients. Export reconstructs selected objects.
The adapter accepts only complete, trusted, quiescent local fixtures. It is not a concurrent
R2 snapshot reader; source copying is not a production transaction or a defense against
an adversarial local filesystem writer.

In a hosted implementation, Postgres/Heddle admission, visibility and retention remain
canonical and R2 retains canonical native bytes. The Artifacts catalog is a derived
publication index, never an authorization grant. A hosted resolver must retain all source
objects for a pin's lifetime and check current authority on every HTTP request. A changed
or unavailable source fails closed; old published pins remain valid only while their exact
scope and policy epoch remain authorized.

## Why the wrapper is Python

This is a narrow standalone prototype package around a public Rust projection boundary,
not the proposed all-Rust `heddle-git-gateway` crate. Python's standard library provided
HTTP/CGI process control and a small catalog test harness without changing workspace
Cargo dependencies. Native state/admission/signature/visibility checks and object export
remain in Rust. This was an implementation tradeoff, not a claim that Python fulfills a
production Rust gateway. Porting HTTP/auth/catalog interfaces to Rust remains code-only
work; it is not necessary to rewrite native storage.

## Run on a clean Mac or Linux machine

Requirements: Git with `git-http-backend`, Python 3.11+, Node 22+ for Worker contract tests,
and Rust 1.98.x with a compiler/linker. The official workspace pins 1.98.0; this Mac's
recorded build used 1.98.1. Public dependencies are locked in the unchanged `Cargo.lock`.
No Python or Node packages are needed for local tests. Cargo initially downloads dependencies.

From the Heddle checkout containing this patch:

```sh
cargo build --locked -p heddle-git-projection --example gateway_native
export GATEWAY_NATIVE="$PWD/target/debug/examples/gateway_native"
export HEDDLE_HOME="$PWD/prototypes/git-gateway/.demo/rust-test-home"
cargo test --locked -p heddle-git-projection gateway_view
python3 -m unittest discover -s prototypes/git-gateway -v
node --test prototypes/git-gateway/worker/index.test.mjs
```

If using `CARGO_TARGET_DIR`, point `GATEWAY_NATIVE` to that directory's
`debug/examples/gateway_native`. Do not run `cargo install`; no installed binary is changed.
See [AUDIT.md](AUDIT.md) for the adversarial review and expanded local test evidence.
The E2E tests bind an ephemeral loopback port and may need sandbox permission for that.
They generate and remove their own synthetic repositories.

For a retained runnable demo, choose a **new** output directory:

```sh
cd prototypes/git-gateway
python3 -m gateway.demo --binary "$GATEWAY_NATIVE" --out .demo/run
python3 -m gateway.http .demo/run/config.json --port 8042
```

In another terminal, substitute the actual `views.base.pin` and `views.merged.pin` from
`.demo/run/evidence.json`:

```sh
git -c 'http.extraHeader=X-Demo-Reader: demo-reader' clone \
  http://127.0.0.1:8042/views/BASE_CATALOG_COMMIT.git demo-clone
git -C demo-clone -c 'http.extraHeader=X-Demo-Reader: demo-reader' fetch \
  http://127.0.0.1:8042/views/MERGED_CATALOG_COMMIT.git refs/heads/main:refs/remotes/view/main
git -C demo-clone checkout --detach refs/remotes/view/main
git -C demo-clone fsck --strict
```

`X-Demo-Reader` is a **test selector, not a credential**. Any local process can set it.
The gateway intentionally binds only `127.0.0.1`, accepts only its local Host names,
and refuses browser-origin, authorization and proxy-forwarding headers. It must never be used with private or
customer data. Its fixture policy file demonstrates exact repository/source/Thread/state/
epoch scoping and revocation mechanics; it does not implement production authentication.

## View and history semantics

`base` and `merged` are complete authorized **derived snapshots**, each with a single root
Git commit. They are not shallow clones of original Git history and do not preserve a
source commit OID from some prior Git import. Fetching a newer pin supplies a different
root; use a new tracking ref or an explicit non-fast-forward update.

`merged` combines the two disjoint worker files by a normal native capture on `main`.
It does not claim an automatic merge of the worker Threads. The independent `ordered`
fixture uses a native merge snapshot on `main`, with ordered parents `[derived view, base]`.
Its history export tests parent order and `git fsck`. Bounded all-public, complete, admitted
native history is implemented. Cross-Thread projection is narrowly supported for
signed fork-base and exact admitted `LocalIntegration` edges under unchanged,
identical local-key ownership, with every governing/dependency Thread explicitly
authorized by the trusted caller. A matching key or signature is not a reader
grant. See [`CURRENT-RUN.md`](CURRENT-RUN.md) for the separate actual-agent
rehearsal, which uses reviewed manual composition rather than automatic merging.
Arbitrary imported Git fidelity/residuals, partial or visibility-filtered
histories, cross-account authority and automatic cross-Thread integration are
not supported by this gateway.

Git OIDs are deterministic **for the same immutable native source and projection mode**.
A newly generated fixture has fresh native IDs/timestamps/signatures and need not have
identical OIDs. The synthetic catalog uses fixed Git identity/time to make publication
replay deterministic from manifest bytes and the expected parent. A production publisher
would persist its chosen commit metadata once per publication transaction.

## Bounds and failure handling

The Rust preflight permits at most 128 states, 10,000 tree entries across the selected
ancestry, depth 64, 16 MiB per blob and 64 MiB aggregate content. The local adapter limits
native metadata copying to 20,000 entries/96 MiB. The catalog caps a manifest at 16 KiB.
HTTP bodies are capped at 1 MiB; child processes have deadlines, CPU/open-file/file-size
limits; backend output is file-backed and capped at 96 MiB. The server processes one
request at a time with a socket timeout and a bounded backlog. These are conservative
prototype ceilings, not production multi-tenant capacity management. No process memory
sandbox, fleet admission controller, streaming large-repo implementation or global quota
manager is provided. This Mac rejected an attempt to set `RLIMIT_AS`; the prototype does
not claim a portable hard memory cap. A malicious compressed native input could exhaust
memory before a post-decode bound is checked. This is one reason the resolver is confined
to trusted synthetic fixtures. A hosted deployment needs OS/container memory and total disk quotas.

Catalog publication is local only. A failed CAS cannot make a losing manifest resolvable.
Unreferenced candidate Git objects may remain until an operator performs normal local Git
maintenance. Retrying the exact bytes and expected parent returns the same already-published
commit, including after a later publication has advanced the branch. Old pins are immutable;
mutable policy epochs can revoke access without rewriting the catalog.

## Artifacts read contract and publication membership

Both native-side catalogs implement `resolve(pin) -> validated manifest`. The local Git
catalog derives membership from its published history. `LocalCatalog.published_pins()`
exports that linear publication history as a bounded, immutable configuration snapshot;
`gateway.demo` writes `published-pins.json`. The REST and Worker adapters require that
trusted allowlist before reading a commit, so merely finding a Git object cannot authorize
an unpublished CAS loser. New catalog publications remain unreadable through Artifacts
until the publication list is deliberately advanced. This is a conservative MVP publication
mechanism, not an automatic hosted publisher or a replacement for current reader authority.

For the Worker, the snapshot is the JSON string `PUBLISHED_CATALOG_PINS`. Neither caller
headers nor manifest contents can extend it. Updating deployed configuration is not part of
this local task. Local contract tests cover real Blob/null and raw HTTP response shapes,
errors, size/encoding checks, canonical parsing and rejected pins. A separate Git clone test
substitutes the REST adapter into the actual gateway using the local catalog as a **response
fixture**. This proves the code/interface seam, not Cloudflare service availability.

## Historical read-only Cloudflare contract and validation checklist

Official documentation checked on 2026-10-01:

* [Workers binding](https://developers.cloudflare.com/artifacts/api/workers-binding/):
  reads and repository lifecycle, Blob-returning reads, disposable capabilities. Generate
  actual environment types with Wrangler 4.145.0+ before live validation.
* [Git protocol](https://developers.cloudflare.com/artifacts/api/git-protocol/): catalog
  commits are published by standard Git smart HTTPS, not an invented binding write method.
* [isomorphic-git Worker example](https://developers.cloudflare.com/artifacts/examples/isomorphic-git/):
  the official option for constructing/pushing commits inside Workers.
* [REST reads](https://developers.cloudflare.com/artifacts/api/rest-api/): `/file` returns
  raw `application/octet-stream` bytes. The native adapter does not invent a JSON envelope.
* [Limits](https://developers.cloudflare.com/artifacts/platform/limits/): 32 MB per blob,
  1 GB per repository. Store metadata in the catalog, not native packs or bulk blobs.

`wrangler.jsonc` is an **undeployed example** with placeholders. Live work still requires
explicit approval and real configuration:

1. Select an existing authorized namespace/catalog and read-only account inspection scope.
2. Approve the exact published-pin snapshot and a pre-existing read credential for the native
   REST adapter if it will be used. Generate binding types and run a local Worker with the approved binding; verify actual
   `readFile` null/error/Blob behavior and capability disposal.
3. Approve creation, if needed, of a small synthetic Artifacts repository and narrowly scoped
   publication access. Publish the already-built local metadata catalog by Git HTTPS (or
   adapt the official isomorphic-git example). No catalog writer is currently live-wired.
4. Implement `AUTH` and `GATEWAY` service bindings. AUTH must authenticate a real caller,
   verify publication membership and exact current scope; the native gateway must
   independently authorize the same pin, verify source authority, and ignore identity
   claims in caller headers. No such hosted services are included here.
5. Implement an R2/native source adapter plus Postgres/Heddle authority snapshot/retention
   contract. Verify original signatures, current revocation, complete authorized closure,
   native hashes, races, redaction and retention loss. No hosted SQL has been run.
6. Only with deployment approval, test ordinary Git through the deployed Worker, including
   revocation between discovery/POST, forbidden object wants, concurrent publication and
   lost acknowledgment recovery. Measure actual CPU/memory/disk bounds and catalog limits.

Workers and Artifacts are represented by runnable Worker source and a real local Git catalog;
**live integration and competition eligibility are not established**. Production Weft wiring
is intentionally optional and absent. No private repository is required to build or run.

### Native bundle / R2 preparation (not live-verified)

`gateway/native_bundle.py` adds a transport-independent `NativeBundleSource`.
`pack_fixture(native_repository)` builds a deterministic, uncompressed <=8 MiB
synthetic `.heddle` archive, omitting signing identity and Git projection caches.
A trusted configuration pins the bundle SHA-256 independently of the manifest;
`read_bundle(source)` is injected. Each request verifies, safely extracts, verifies
native signatures/visibility through the Rust exporter, then reconstructs Git in
new temporary storage. No Git repository cache is needed. This is a fixture
transport format, not a production Heddle/Postgres replication format.

`worker/native-source.mjs` uses the documented R2 `get()` / `arrayBuffer()` read
contract and validates size/digest. It is an internal prepared component, **not
wired into the public router or a native host**. No R2 uploads/downloads have been
performed. Tests use local byte callbacks and R2-shaped response fixtures. Native
host deployment, real authentication, Worker-to-native transport and production
R2/Postgres authority integration remain unfinished.

Run the added tests alongside the earlier suite:

```sh
GATEWAY_NATIVE=/absolute/path/to/gateway_native python3 -m unittest -v test_native_bundle test_gateway test_artifacts test_process_bounds
node --test worker/*.test.mjs
```

The bundle test deletes its original native repository before ordinary Git
clone/fetch, compares both concurrent edits and projected OID, and runs strict
`git fsck`. Each HTTP request reloads the archive and reconstructs fresh Git.
This proves a local cache-cold transport boundary; it does not prove live R2 or
Artifacts operation. Credentials and live account inventory belong outside this
public source package.

### Authenticated transport preparation (local contracts only)

`gateway/auth.py` now supplies opt-in bearer authorization from an externally
provisioned JSON policy. Each reader has `sha256` (credential digest), `expires_at`
(Unix seconds), and `views` (exact `[repository, source, thread, state, policy_epoch]`
arrays). Policy reloads on every GET/POST. No credential is minted or included in
configuration; tests use an explicitly public constant. Call `server(...,
bearer_mode=True)` with `BearerAuthorization`; the default CLI remains fixture-only.
Both modes still bind only to loopback, enforce local Host, and reject forwarding
headers. A production listener/TLS proxy is deliberately not enabled.

`gateway/bundle_transport.py` supplies `HTTPSBundleReader`: a fixed HTTPS origin,
pre-provisioned service credential, no redirects/proxies/retries, a 15-second
request timeout and <=8 MiB raw response. It can be injected into
`NativeBundleSource`. `nativeBundleService` in `worker/native-source.mjs` implements
the counterpart GET `/native/<logical-source>` using R2, with a mandatory injected
service-authorization function before storage access. It is not routed/deployed.
The implementation never accepts source URLs or R2 keys from the manifest/client.

Local tests exercise the Python transport with an injected HTTP response and
ordinary Git clone with bearer authorization, then revoke the policy and confirm
Git access fails. Worker tests exercise the matching private service contract and
authorization-before-R2 ordering. **No actual HTTPS service, R2, Container, or
Cloudflare identity provider has been validated.** Hosted listener isolation,
TLS/service identity, credential provisioning, native image packaging, hard memory
limits, and live binding wiring remain outstanding. This is an executable local
boundary, not a completed hosted deployment.

```sh
GATEWAY_NATIVE=/absolute/path/to/gateway_native python3 -m unittest -v test_authenticated_transport test_native_bundle test_gateway test_artifacts test_process_bounds
node --test worker/*.test.mjs
```
