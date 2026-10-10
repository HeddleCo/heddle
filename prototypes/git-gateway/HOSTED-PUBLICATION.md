# Hosted native Git publication

This local implementation extends the frozen preparation-only checkpoint. Cloud
activation, enrollment and a live Artifacts repository remain separate gates.
Do not deploy this prototype using test fixture credentials or identities.

## The success boundary

The stable URL is `/repositories/<registered-slug>.git`. Stock Git can use an
opaque short-lived Git session as an HTTP Basic password with username `git`,
or supply `Authorization: Bearer <session>` using `http.extraHeader`. There is
no special write header. `/git/auth/challenge` and `/git/auth/exchange` carry
the protobuf proof-of-possession exchange. A session is a transport carrier;
it never substitutes for the independently configured gateway publisher.

The Worker passes only the user grant, Git protocol/content type, and its
separately configured internal service credential. Rust resolves the stable
repository to exact tenant, Spool, Thread and disclosure scope. Current Weft
session and full-history authorization run before and after expensive work.
The hosted path currently reconstructs per request; it has no reconstruction
cache. Any future cache must preserve these authority checks. This window
currently accepts public disclosure only.

A successful receive follows this order:

1. Parse one bounded fast-forward branch update and reconstruct exact Git
   objects in a disposable quarantine. Prepare genuine native originals and
   separate SourcePacks for every historical revision.
2. Persist the token-free native plan and all native bytes in private R2
   staging. The manifest is written last. No incoming Git pack is retained.
   Staging consumes exact owned proof/source ArrayBuffers before its first await,
   detaching caller views even if later work fails. Shared, slab and subarray
   views are refused; mutation cannot change a validated immutable upload.
3. Record a durable publication intent. Send the complete history using the
   dedicated `PublishContent` Git acceptance feature, requiring protocol 2 and
   mandatory features `[1, 2]`. An older receiver is refused before uploading.
4. Obtain the receiver's atomic native/Git expected-old acceptance receipt.
   Ambiguous acceptance retries use the same original native operation UUID
   and semantic request digest, with a fresh authorized session.
   Every upload receipt binds the exact submitted inventory. A separate ordered
   `retained_history` mapping identifies the receiver's actual durable native
   inventory and Sharing frontier for the same States. A redundant ancestor
   repack may differ physically; it never overwrites the existing source marker.
5. Verify every exact content-addressed native pack/index in durable R2.
6. Publish the small canonical manifest as a deterministic Artifacts Git commit
   using an expected-old ref update. The Workers Artifacts binding is used only
   to read the immutable manifest back, not as a fictitious content-write API.
7. Recheck the current native head/generation and full current authority, then
   send the ordinary Git `unpack ok` and branch `ok` report.

Native acceptance cannot be rolled back by a catalog failure. Pending work
remains durable, and ordinary receive discovery repairs it before advertising a
head. Reads cannot skip pending work or serve an older cached catalog. Changed
native head/generation, revocation, absent history authority, missing bytes,
corruption, expected-old conflicts, and malformed receipts all fail closed.

## Bootstrap is separate

An empty catalog can initialize from an explicitly configured, already admitted
native head. The host checks the real native history inventory, fetches and
validates every native revision, projects the fixed shared recipe, and creates
a parentless metadata catalog commit under expected-absent CAS. This uses an
explicit `native-bootstrap` proof and receipt kind. It has no Git acceptance
command, cannot call the native push sender, and is never a push ACK. Initialization requires a write-scoped session; a first reader receives 403
until a writer initializes the catalog. Initialized reads accept independent
read-scoped sessions.

## Concrete composition

`worker/native-http.mjs` composes `hostedGateway`, `R2NativeStaging` and
`PublicationCoordinator` against the real bounded Rust `/native/v1` bridge.
The per-repository Durable Object supplies its own transactional storage and
R2 binding. Trusted configuration supplies exact repository-to-catalog names,
initial native heads, the internal service binding and its existing credential.
`NATIVE_SERVICE_AUTHORIZATION` is the complete `Bearer <token>` header. The token
must be 32–256 printable, non-space ASCII bytes. The controller forwards that
header unchanged and configures Rust with SHA-256 of the token alone, excluding
the `Bearer ` prefix. Configuration validation never creates this credential.

The bridge methods are `authorize`, `bootstrap-plan`, `bootstrap`, `refresh-plan`,
`refresh`, `prepare`, `validate-plan`, `submit`, `project`, `catalog-publish`, and
`reconcile-inspect`. Source artifacts and metadata proofs use bounded HGF1 raw
binary frames with a small canonical JSON header; arbitrary URLs and caller
identity headers are not forwarded. The Rust host must be configured with its
existing scoped publisher and Artifacts credentials by the operator. The
prototype does not create or enroll any real identity.

Private staging keys are scoped by tenant, Spool and semantic journal digest.
Their only contents are native SourcePacks/indexes and metadata proofs. The
proof retains the original native operation UUID independently of the Worker's
SHA-256 journal key. Post-acceptance authority checks use this exact UUID to
resolve the actual receiver receipt. A bootstrap lookup at the new head cannot
substitute for that receipt.

## Accounting dimensions

Receipts distinguish authenticated actor, gateway signing key and the existing
Spool billing owner resolved by current native authority. Native R2 bytes and
Artifacts manifest bytes are separate dimensions. Projection caches are
operational overhead. Values report logical referenced content, not an invoice;
this work changes no prices, billing ownership, or payment settings.

## Verification scope

The Node security suites exercise real edge/transport composition and failure
boundaries with explicitly controlled service adapters. The Miniflare suite
uses actual local workerd, R2 and SQLite Durable Object storage across runtime
restarts. Rust tests exercise actual native packs, bounded proof restoration,
shared fixed-recipe projection and real Git metadata expected-old CAS.
An end-to-end claim additionally requires the concrete Rust host and actual
local Weft/Postgres acceptance path; adapter fixtures alone are not that proof.
None of these local suites establishes live Artifacts or cloud deployment.

The internal bridge uses bounded HGF1 frames: a canonical JSON metadata header
followed by raw native bytes. Native parts are decoded into exact-size arrays;
Git output remains a bounded stream. The Container controller applies one
`FixedLengthStream` immediately before its HTTP socket hop, avoiding chunked
transfer and duplicate buffering. Worker catalog/source checks run before
projection; Rust rechecks current full-history authority after construction and
before sending any output frame bytes. No second native RPC blocks an unread
Git output stream.

The concrete Cloudflare entrypoint is `worker/cloudflare/hosted-index.mjs`, with
`HostedPublication` as its per-repository SQLite Durable Object. Its source-code
activation gate is false. `wrangler.hosted.draft.jsonc` is a review template,
not an instruction to provision or deploy anything. The exact registry lives
in `HOSTED_REPOSITORIES`; empty or unknown values deny. Missing Git credentials
receive the Basic challenge required by stock Git credential helpers.

The deploy entrypoint `hosted-deploy.mjs` exports the concrete `HostedNativeGateway`
Container class. The Worker invokes its fixed named instance through the actual
`NATIVE_CONTAINER` binding. Public PoP challenge/exchange are forwarded only to
the exact HTTPS `authority_origin` in trusted native configuration. No caller
URL or identity header chooses that destination.
This first hosted configuration permits exactly one registered repository and
one fixed named native Container; the publication journal remains repository-scoped.

Native Iroh currently requires general QUIC-capable egress. The Container's
activation gate and a separate native-network review gate both default false;
`enableInternet` is false by default. Only a future explicit code review of both
gates allows startup with general internet access. HTTP `allowedHosts` rules do
not constrain arbitrary QUIC, and this controller makes no such claim. The Rust
client still pins its configured Weft descriptor and Artifacts HTTPS destination.
See the [official outbound traffic contract](https://developers.cloudflare.com/containers/configuration/outbound-traffic/).

The hosted image contains code only. Startup writes operator-supplied existing
credential material to private ephemeral files, removes the secrets from the
child process environment, and starts the concrete Rust host. No credentials
are generated or enrolled. The image build context includes the exact sibling
`heddle/` and `api/` source trees; the Docker-specific allowlist excludes local
stores, secrets, binaries and logs. Local bundle/controller tests do not activate
or demonstrate a live Container, Artifacts account, or network policy.

## Native changes after initialization

An initialized native Thread can advance without a Git push, including a metadata
operation that changes its generation but keeps its State. The old catalog then
fails current authority checks. It is not silently accepted or reinterpreted as
a successful push. An authorized writer may explicitly POST
`/repositories/<slug>.git/native-refresh` with an `application/json` body containing
exactly `expected_native` (the currently selected native head) and
`expected_catalog` (the previous immutable catalog pin).

The distinct schema-3 refresh intent and `native-refresh` receipt use current
server generation, fresh full native-history authority, exact projection and
expected-old catalog CAS. A pending push cannot be replaced. The prior catalog
metadata is authenticated as the CAS fence without reading obsolete source
bytes or requiring its obsolete native generation. The new closure is freshly
authorized and verified. Refresh returns metadata JSON, never Git receive-pack
success. An unavailable Git view includes an `x-heddle-recovery` response header
pointing to explicit receive-discovery recovery or native refresh as applicable.

## Explicit reconciliation after a native writer advances

A separate native writer can advance the Thread after Git acceptance but before
catalog publication finishes. Ordinary recovery keeps the original receipt's
generation fence and cannot ACK that stale push. The original authenticated
writer may explicitly POST `/repositories/<slug>.git/native-reconcile` with
`expected_operation`, `expected_native`, `expected_generation`, and
`expected_catalog`. All four are exact observed fences, not replacement values.
This is an operator recovery API: obtain the exact operation from the per-repository
journal, current native State/generation from native authority, and actual catalog
pin from the configured metadata ref. The generic Git error header names these
actions without disclosing journal contents. Ordinary transient failures still
repair through receive discovery; competing native changes are not automatically
merged or acknowledged.

The runtime verifies the original durable receiver receipt, retained old-history
visibility, freshly current full closure and original writer. It reads the actual
Artifacts ref and accepts only the prior catalog or the deterministic pending
manifest commit, with exact immutable metadata verification. A third ref value,
changed current head/generation, another writer, or unavailable authority denies.

One Durable Object transaction preserves an immutable `accepted-superseded`
audit and removes the exact pending journal. If the pending manifest already
reached Artifacts, its verified metadata becomes the next catalog CAS base.
Nothing claims that stale source is currently readable. The response is audit
JSON, never Git success; original push retries cannot ACK the superseded command.
The writer can then use the existing explicit native-refresh route to publish
the newly current view. Crashes before the transaction preserve pending work;
crashes after it preserve the terminal audit and a refreshable catalog base.

For an explicitly unresolved command, the writer may instead include
`outcome: "unresolved-superseded"`. This is never selected because a lookup failed.
It requires a prepared journal with no known receipt, a verified current native
generation strictly above the pending expected generation (so that old CAS
cannot newly commit), and the actual remote catalog still exactly at the prior
pin. The complete pending identity is retained with acceptance `unknown`, not
rejected or accepted. A changed remote ref is refused because no deterministic
pending commit can be established without the actual receipt. This terminal
outcome also returns only audit JSON and never allows a Git ACK.
