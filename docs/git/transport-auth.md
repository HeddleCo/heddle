# Git transport authentication

Status: foundation in place; local code and tests, no live enrollment or deployment.

Ordinary Git cannot produce Heddle's request-key proof. A keyed Heddle client
therefore exchanges its existing credential for a separate opaque, short-lived
Git HTTPS credential. A Biscuit alone is never accepted as a Git bearer.

## Exchange

The shared API messages are in `git_transport.proto`. Weft exposes protobuf
POST bodies at `/git/auth/challenge`, `/git/auth/exchange` and
`/git/auth/authorize`, with a 64 KiB HTTP body cap and `Cache-Control: no-store`.
The Heddle `GitTransportClient` uses HTTPS only, ignores ambient proxy settings,
and does not follow redirects. Standalone construction initializes the existing
ring TLS provider convention without replacing an already installed provider.
It does not enroll a root, save a credential or put a token in a URL.

The scope binds:

- service audience `git-gateway`, separate from source disclosure audience
- stable tenant/parent Spool UUID and content Spool UUID
- the registry-resolved canonical repository path
- immutable 32-byte Thread identity
- exactly `read` or `write`
- existing disclosure audience (`public`, `internal`, `team:…`, `restricted:…`)

The server creates a random 32-byte challenge, valid for at most 60 seconds.
The effective PoP leaf key signs the versioned, length-delimited encoding of
all scope fields, the exact Biscuit/envelope digest, challenge nonce and times,
and requested session expiry. The shared verifier supplies the canonical bytes
and verifies the Ed25519 signature strictly. Agent operation/resource/time checks
and signed child-key transitions are verified by the ordinary Biscuit verifier.
`git_transport_request_v1` is verifier-only: token facts and rule heads cannot
assert it. A token can narrow itself with checks against that predicate.
The ordinary request resource remains the canonical Spool, as on existing native
hosted calls. A Thread-only `AgentAttenuation` resource-kind ceiling therefore
fails closed here; use a check on the exact Git request predicate to narrow a
Git credential to its immutable Thread without changing native resource semantics.

Current Weft accepts registered roots, not grant envelopes. It rejects an
unowned pinned key; token `user(...)` facts cannot create an account identity.
The registered root's current owner is overlaid, all relevant revocations are
checked, and the current Thread/Spool role and disclosure are resolved from the
registry. Identity-only account roots are supported: empty token rights do not
grant authority; live hosted grants still must admit the request. Explicit
root-origin rights, when present, impose an additional ceiling.

One shared PostgreSQL transaction locks the authorization epoch, atomically
consumes the exact challenge, and stores the session hash. Concurrent authority
replicas cannot consume the same nonce twice. The token is `ggit1_` followed by
32 random bytes encoded as lowercase hex. Its maximum lifetime is five minutes,
shortened to any earlier relied-on grant expiry. Only its digest is persisted. Each challenge request also removes at most
256 expired session rows, skipping locked rows and preserving live grants, to
limit retention of their original PoP-bound credentials.
Use it as a transient HTTPS bearer (or a gateway-supported Git HTTP Basic
password with username `git`). This is intentionally a bearer grant within its
short lifetime; TLS and secret handling remain required.

## Every request and native acceptance

`authorize_git_transport` reloads the session and original Biscuit, catches up
the durable authorization-event feed, rechecks registered ownership/revocations,
re-evaluates all Biscuit checks, and resolves current grants/Thread disclosure.
An epoch, actor, canonical path or scope change rejects the old session. Natural
grant/session expiry also rejects, even without an epoch change.

`/git/auth/authorize` compares every canonical scope dimension exactly, except
that a write transport session may narrow to read for Git negotiation/fetch.
Its original Write caveats and current writer role are still revalidated; a read
session cannot become a write session.
Its response is session authority only. It is **not** proof of the selected
history's per-State/entry visibility, redaction, retained admission or ownership.
The gateway must separately resolve that complete immutable source closure
before reading projection caches, publishing metadata or acknowledging Git.
Stable route labels must come from an explicit trusted catalog mapping, never
from suffix matching or caller-provided actor headers.

Native publication continues to use a separately enrolled/delegated gateway
`SourceAuthor` and its own request-key proof. The original Git client's Biscuit
is never spliced onto the gateway's signing key. The receiver resolves the opaque
transport token itself and accepts only a privately constructed
`AuthorizedGitSession`. It rechecks this guard under the same SQL authorization
epoch fence as native admission. Actor identity is registry-derived; retry
identity is the immutable operation, not the short-lived Git token.

## Explicit operator exchange command

The standalone `git_transport_session` example reuses `GitTransportClient` and
the existing `Ed25519Signer`; callers do not implement signing or nonce handling.
It never discovers credentials, enrolls an identity, renews a grant automatically,
or updates Git configuration. Build from the Heddle checkout:

```sh
cargo build -p heddle-hosted-client --features client --example git_transport_session
cargo test -p heddle-hosted-client --features client --example git_transport_session
```

The executable is under `${CARGO_TARGET_DIR:-target}/debug/examples/`. Its scope
file is strict JSON with all dimensions explicit, for example (synthetic IDs;
replace them with the approved registry's exact existing scope):

```json
{
  "service_audience": "git-gateway",
  "tenant_spool_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
  "spool_id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
  "repository_path": "example/repository",
  "thread_id": "1111111111111111111111111111111111111111111111111111111111111111",
  "action": "write",
  "disclosure_audience": "public"
}
```

For an already authorized deployment and existing credential files:

```sh
"${CARGO_TARGET_DIR:-target}/debug/examples/git_transport_session" \
  --authority https://approved-authority.example \
  --scope-file /explicit/scope.json \
  --biscuit-file /explicit/existing.biscuit \
  --proof-key-file /explicit/existing-ed25519-pkcs8.pem
```

**Stdout is deliberately the secret session token followed by one newline.**
Send it directly to your approved transient Git credential mechanism; do not
record the terminal, log stdout, include the token in a URL or command argument,
or save it in Git configuration. Secret input files require owner-only Unix
permissions; all inputs must be regular files, not symlinks. The proof key must
be the existing Biscuit's effective Ed25519 key. Failure emits only a generic
diagnostic and a nonzero exit status. No credential material is logged.

Use the result as Git's transient HTTPS Basic password with username `git`.
The command requests a four-minute session; current grant expiry may shorten it.
Run it again explicitly when renewal is needed. Write scope also permits read
negotiation on that same exact repository/Thread. No stock-Git helper is installed.

The public gateway forwards only the challenge and exchange endpoints. Calling
the library's optional `authorize` helper requires the Weft authority origin;
that endpoint is not exposed by the public gateway. None of these examples
authorizes real credential enrollment, secret configuration or deployment.

## Explicit gateway native publisher

`hosted_client::gateway::GatewayClient` takes an explicit `ClientConfig`, pinned
deployment descriptor, existing account `SourceAuthor`, and its matching proof
key. It never loads ambient credentials. The configured Biscuit and sealed
SourceAuthor must have the same authenticated complete chain and effective key.
A fresh native GetIdentity binds current account, agent, proof key and chain to
independently observed owner history and the admitted mint-root association.

`refresh_source_authority` produces a 30-second, exact-Spool/Thread/path snapshot
for new gateway-original operations. It checks current PublishContent caveats and
refuses other publishers or authors. This is preparation evidence, not a durable
DB write grant: every historical original still needs the separate exact-closure
authority response, and the receiver rechecks final write admission under its
transaction fence. `hydrate_exact` reuses authenticated native Fetch and the
existing full-closure importer into a caller-created keyless clone, validating
exact Spool, Thread, State and complete closure before install.

## Private native bridge framing

The Worker/native bridge uses `application/vnd.heddle.native-frame-v1` at
`POST /native/v1`. Its `HGF1` frame is four magic bytes, a big-endian 32-bit
header length, the canonical JSON header (sorted object keys and terminal LF),
then raw parts in descriptor order. The header contains exactly `payload` and
`parts`; each part has exactly `name` and unsigned `length`. Source bytes are
never base64-encoded in this bridge. Ordinary Git HTTP wire messages are unchanged.

The header is at most 256 KiB; the entire request is at most 112 MiB and response
at most 144 MiB. Up to 256 artifact parts plus `proof`, `request`, `output` and
`manifest` are allowed, for 260 total parts. Proof and request parts are at most 17 MiB each, output at most 96 MiB,
and individual and combined artifact bytes at most 64 MiB. Manifest bytes are
at most 64 KiB. Existing per-method admission limits remain narrower where needed.
Artifact names are `artifact/<lowercase SHA-256>`. Payload `proof_part`,
`request_part`, `output_part` and `manifest_part` markers name the corresponding
part. Artifact records use `sha256` with an exactly matching `bytes_part` marker.
Unknown markers, duplicate descriptors, missing or unused parts, noncanonical
headers, and extra or truncated bytes fail closed. Repeated references may share
one artifact part. All response layout checks precede the HTTP success status;
headers and raw parts are written sequentially under one bounded I/O deadline.

## Local verification

- Shared verifier unit tests: signature/request binding, expiry/audience,
  delegated leaf ownership and ancestor ceilings, reserved fact/rule forgery
- Independent verifier and registered-root security tests
- `weft-hosted --test git_transport_exchange`: real PostgreSQL account/root/grant
  and Thread fixtures; concurrent nonce replay across two authority instances,
  wrong proof, scope/label denial, grant downgrade, root revocation and expiry

PostgreSQL tests require an explicitly configured local `TEST_DATABASE_URL`.
A compiled or ignored test is not an executed integration result.
