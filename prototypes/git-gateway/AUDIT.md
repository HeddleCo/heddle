# Adversarial review and actual implementation boundaries

For the subsequent cloud-Linux host, independent service/reader authorization,
hard child memory-limit checks, HTTPS transport adapter and unexecuted container
profile, see [HOSTING.md](HOSTING.md). Historical Mac limitations and evidence
below describe their original checkpoints and are not rewritten as live proof.

All validation here is **local**, on official Heddle base
`ecf70cc15c9f7ffc620345ea0a8d8c007ab2eb7c`. No deployment, external writes, new cloud
credentials, hosted SQL, or PRs were performed. The executor remained usable after a
transient disconnect notification; the running test session completed once, without a
duplicate test launch.

## What is implemented, versus proposed

| Boundary | Actual implementation | Not implemented or verified |
| --- | --- | --- |
| Native projection | Public Rust `gateway_view` module in the existing git-projection crate, plus `gateway_native` adapter binary | A new all-Rust gateway crate |
| Protocol serving | Python standard-library HTTP wrapper, fresh per-request bare repository, native `git-http-backend` | Production HTTP/TLS/multi-tenant server |
| Local catalog | Real Git manifest commits, CAS publication and replay, `resolve(pin)` | Live Artifacts publication |
| Native-side Artifacts reads | Real REST `ArtifactsCatalog.resolve(pin)` using documented raw file response, bounded HTTPS read and trusted publication snapshot | Actual account responses or deployed gateway configuration |
| Worker | Executable route/auth-call/body-limit/proxy code and real binding `get`/`readFile`/dispose adapter | Deployed Worker, AUTH service, GATEWAY service binding |
| Canonical source | Real synthetic native Heddle objects, signed operations, admission and privacy metadata | R2/Postgres/Weft adapter and hosted authorization snapshot |
| Identity | Deliberately local fixture selector with exact view scope | Production authentication or revocation service |
| Concurrent work | Two actual native Threads with separate materialized checkouts and concurrent synthetic workers | Remote LLM-agent demonstration or automatic cross-Thread landing |

Python was chosen to keep the HTTP/CGI/catalog harness small and dependency-free without
changing Cargo dependencies. That is a departure from the preferred all-Rust package, not
a storage or Git-protocol substitution. Porting the wrapper can preserve the same boundaries.

The actual native read path is:

1. Resolve and validate an immutable manifest; authorize exact fixture scope.
2. Copy bounded native metadata, excluding its signing key, to an isolated directory.
3. `Repository::open` that copy; verify the governing native Thread, admitted originals,
   original signatures, current/original privacy and full ancestry. Reject unsafe history.
4. Invoke existing `export_state` → `export_tree`, with fresh `SyncMapping` and fresh Sley
   sink. The existing exporter reconstructs blobs/trees/commits. No old Git mirror,
   exported pack or mapping is required or installed in the sink.
5. Compare the reconstructed commit OID to the pinned manifest, set one advertised ref,
   run Git fsck, and serve through native upload-pack.

The snapshot adapter rejects imported/fidelity states, so it does not claim support for
`reconstruct_commit_bytes` on imported Git history. That existing upstream routine remains
untouched. Ordered native-history projection is tested separately from the derived snapshot.

## Defects found and fixed in this review

* **Artifacts interface parity:** the initial Worker reader parsed a Blob and checked only
  two fields, leaving publication membership to an unimplemented service. It now requires
  a trusted exact-pin list and full shared canonical validation. Added the native-side REST
  adapter with the exact same `resolve(pin)` consumer interface. A real ordinary-Git clone
  now runs with this adapter substituted for `LocalCatalog`, using documented-response
  fixtures backed by the local catalog.
* **Ambiguous JSON:** duplicate keys, whitespace variants, unknown fields, bool schemas and
  unsafe integer epochs now fail consistently in Python and JavaScript. The publication
  format is canonical ASCII JSON with a final newline.
* **Accidental remote fixture use:** loopback binding alone was insufficient to reject a
  reverse-proxy/edge origin. The reference server now also checks exact local Host values
  and peer address, refuses Origin/Authorization/forwarding headers, and rejects duplicate
  fixture selectors. The Worker strips fixture headers, and pointing it directly at the
  fixture server fails closed. This is not a guarantee against an operator deliberately
  tunneling requests while rewriting all those attributes.
* **Absolute/ambiguous request targets:** absolute-form URLs and fragments are refused;
  malformed signed/comma Content-Length values now return 400 rather than backend errors.

## Threat checks and findings

| Concern | Check performed | Result / residual limit |
| --- | --- | --- |
| Path traversal | Encoded separators, `..`, noncanonical routes, logical-source validation, native symlink fixture | Refused before serving/copying |
| Unadvertised object wants | Real Git fetch of a known unserved restricted blob OID; sink object lookup | Fetch fails; object absent from sink. Entire source packs are never sent |
| CGI/env injection | Poisoned GIT_DIR, alternates, exec path, config-count variables plus incoming CGI-like headers | Valid request still serves the correct view. Child environments are constructed, no shell used |
| Mutable publication | Advance catalog head while resolution is paused; concurrent CAS loser lookup | Pinned bytes stay unchanged; unpublished loser refused |
| Artifacts object existence | Valid-looking unknown/unpublished pins | Rejected before any REST/binding I/O |
| Source/cache independence | Poison old mapping, then remove `.git`, `.heddle/git-projection`, worktree state and materialization caches | Same Git OID reconstructed; catalog remains readable even after native copy removal |
| Missing/corrupt source | Missing state, mismatched Git OID, invalid config and actual corrupted native packs | Fail closed; catalog alone cannot replace missing canonical bytes |
| Privacy persistence | Remove mutable entry sidecar after genuine signed capture | Rust check still denies original captured private content |
| Resource limits | Rust zero state/entry/blob/byte budgets; HTTP oversize input; child timeout and >96 MiB output file | Limits exercised. Native input remains trusted, not adversarial arbitrary data |
| Hard memory limit | Isolated attempt to set 1 GiB RLIMIT_AS on this Mac | OS rejected with ValueError. No hard memory sandbox is claimed |

The remaining memory limitation matters: native decoding occurs before some in-memory
content-size checks. An adversarial compressed source could exhaust memory. Hosted use
requires a supported process/container memory quota and total disk admission, plus a
consistent source authority/retention transaction. A mutable local filesystem writer is
also outside this quiescent-fixture adapter's trust model.

## Catalog interface details

`LocalCatalog.published_pins()` exports up to 1,024 first-parent publication commits. The
publisher creates linear history. REST and binding readers hold an immutable copy of this
trusted list. The Worker receives it as `PUBLISHED_CATALOG_PINS`; the REST constructor
receives `published_pins`. A caller cannot nominate a pin merely because it exists in Git.
This deliberately adds an approval/configuration step before a newly published pin becomes
readable through Artifacts. It is not an automatic registry and is not reader authorization.
AUTH/GATEWAY must independently verify current reader scope in a live deployment.

Official contracts used:

* [Workers binding](https://developers.cloudflare.com/artifacts/api/workers-binding/):
  disposable repo handle and `readFile({ref, path}) -> Blob | null`.
* [REST API](https://developers.cloudflare.com/artifacts/api/rest-api/): account/namespace/
  repo `/file?ref=&path=` route returns raw bytes, not a JSON result envelope.
* [Git protocol](https://developers.cloudflare.com/artifacts/api/git-protocol/): writes
  remain Git smart HTTPS. No invented binding write methods or raw-pack indirection.

The REST adapter fixes the API origin, validates configured path components, disables
redirects and environment proxies, caps reads at 16 KiB, and requires an existing supplied
read credential. Tests use only an unmistakable fixture string and injected local transport.
No runtime credential lookup or live request was executed. Both adapter families use the
same checked-in `catalog-fixtures.json` for accepted/rejected manifest bytes.

## Updated results and commands

* **21 Python tests passed**, including 14 gateway/native tests, 5 Artifacts response-contract
  tests, and 2 process-limit tests: [audit-tests.txt](evidence/audit-tests.txt).
* **11 Worker tests passed** using documented Blob/null fixtures and service doubles:
  [worker-audit-tests.txt](evidence/worker-audit-tests.txt).
* The Rust implementation was unchanged in this review; its **3 targeted tests passed** at
  the implementation checkpoint: [rust-tests.txt](evidence/rust-tests.txt).
* Initial authorization mutation evidence is retained in
  [authorization-mutation.txt](evidence/authorization-mutation.txt).

From the task directory:

```sh
GATEWAY_NATIVE="$PWD/target/debug/examples/gateway_native" python3 -m unittest discover -s heddle-current/prototypes/git-gateway -v
node --test heddle-current/prototypes/git-gateway/worker/index.test.mjs
```

The first command used a permitted loopback listener. The second uses no account or network.
Logs saying `LOCAL_FIXTURES` mean exactly that; this is not live Cloudflare validation.

## Remaining work

Code-only work includes an all-Rust wrapper if desired, production AUTH/GATEWAY contract
implementations, R2/native byte retrieval and Postgres/Heddle authority/retention adapters,
and a deployment-specific memory/disk sandbox. Access to real infrastructure is not needed
to design those seams, but validating them against actual accounts requires explicit approval,
authorized read access and then separately approved synthetic publication/deployment.
No task currently requires bypassing visibility, minting credentials, deploying the demo
identity mechanism, or treating a mock as a live integration. Competition eligibility remains
unestablished.

## Subsequent pre-live preparation

Added bounded native bundles and the documented R2 read-contract component.
The bundle end-to-end test removes its original native repository, then clones
and fetches with ordinary Git, checks concurrent fixture edits and OID, and runs
strict fsck. Every request re-extracts and re-projects. Added digest, missing-source,
unsafe archive-entry and R2 missing/size/corruption contract tests. These are local
fixtures, not hosted verification. The initial legacy tar format failed on native
long filenames; deterministic PAX format addresses that without compression or
unbounded extraction. Existing trusted/quiescent-input and hard-memory-limit
caveats still apply. No real authority, native container, service credentials or
live publisher have been added.

Final pre-live results are in `evidence/prelive-tests.txt` and
`evidence/worker-prelive-tests.txt`; these supersede earlier suite counts.

## Authenticated transport follow-up

Opt-in bearer authority reloads exact view scope and expiry on every request;
credentials are provided externally, stored as policy digests, and compared using
`hmac.compare_digest`. The fixture listener restrictions remain in both modes.
Added fixed-origin HTTPS native bundle transport (no proxies/redirects/retries,
bounded response) and its internal R2 Worker handler with mandatory injected
service authorization. The public Worker route still does not expose native packs.
Tests use a public constant and injected HTTP/R2 responses; they do not create
credentials or establish live Cloudflare validation. Results are recorded in
`evidence/authenticated-transport-tests.txt` and
`evidence/worker-authenticated-transport-tests.txt`.
