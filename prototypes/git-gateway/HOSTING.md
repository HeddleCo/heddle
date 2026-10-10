> Preserved historical Python reference host preparation. Current Heddle 0.30.0 hosted authentication, receiver acceptance and publication code is described in [HOSTED-PUBLICATION.md](HOSTED-PUBLICATION.md) and [HOSTED-RUNTIME.md](HOSTED-RUNTIME.md). Statements below about missing hosted interfaces apply to this earlier path, not the current implementation.

# Authenticated native host preparation

The separate [guarded Cloudflare draft](worker/cloudflare/README.md) now includes
an explicit platform-specific internal bridge and authenticated ingress variant.
The generic host and Docker profile described below remain separate and unchanged.

This is code-only preparation, validated locally on non-root Linux. No Cloudflare
resources, account credentials, deployment, live Artifacts/R2 request, TLS endpoint,
or production identity provider has been created or tested.

## Executable composition

The public Worker still authenticates the reader through AUTH and reads the pinned
manifest through its Artifacts binding. `gatewayTransport` can implement its
GATEWAY interface using a fixed HTTPS origin and an existing service credential.
It retains reader authorization separately, injects its own hop identity, strips
caller-supplied identity/proxy headers, rejects redirects, bounds request/response
bytes, and enforces an overall transport deadline.

```javascript
import { gatewayTransport } from './worker/gateway-transport.mjs';
const nativeGateway = gatewayTransport({
  origin: approvedFixedHttpsOrigin,
  serviceCredential: existingServiceCredential,
});
// Supply nativeGateway as env.GATEWAY in the approved deployment composition.
```

The Python host is a separate entrypoint:

```sh
python3 -m gateway.host /config/host.json
```

It requires non-root Linux, retains a **loopback-only** listener, and has no fixture
selector mode or public-bind option. An eventual TLS ingress/sidecar must share its
network namespace, pass requests to 127.0.0.1 with that local Host, preserve both
authorization headers, and omit Origin/forwarding identity headers. Provisioning
and verifying that ingress, TLS and service network isolation remain future work.
The service credential authorizes a specific catalog pin before catalog I/O; the
reader credential independently authorizes the exact repository/source/Thread/
state/policy epoch before native bundle access. Both policies reload on every
discovery GET and upload-pack POST. A valid service identity is never a reader grant.

## Configuration contract

`host.json` has exactly schema, binary, reader_policy, service_policy, catalog,
native, and optional port (default 8042). All paths are container-local. No secret
value belongs directly in this JSON or in the image. Policies contain credential
digests supplied by the operator; separate read credentials use mounted files.
The host never mints, refreshes or provisions credentials.

Example structure for already-approved live read integrations:

```json
{
  "schema": 1,
  "binary": "/usr/local/bin/gateway_native",
  "reader_policy": "/config/readers.json",
  "service_policy": "/config/services.json",
  "port": 8042,
  "catalog": {
    "kind": "artifacts",
    "account": "APPROVED_ACCOUNT_ID",
    "namespace": "APPROVED_NAMESPACE",
    "repository": "APPROVED_CATALOG",
    "published_pins": ["APPROVED_40_HEX_CATALOG_COMMIT"],
    "credential_file": "/config/artifacts-read-credential"
  },
  "native": {
    "kind": "https",
    "origin": "https://APPROVED_INTERNAL_BUNDLE_ORIGIN",
    "credential_file": "/config/native-read-credential",
    "descriptors": {"native-demo": "APPROVED_64_HEX_NATIVE_BUNDLE_SHA256"}
  }
}
```

The placeholders intentionally fail validation. Local tests instead use an
existing read-only Git catalog (`{"kind":"local","path":"/config/catalog.git"}`)
and local native bundles (`kind: "local-bundles"`, the same descriptors map, plus
a source-name→absolute-file-path `paths` map). Neither route initializes a catalog,
publishes commits, writes native source bytes, or chooses source locations from
client inputs.

An optional `native.authorized_threads` map supplies a **complete, explicit trusted
local Thread allowlist per source**, for example `{"native-demo":["main",
"demo/open-filter","demo/priority-sort"]}`. The manifest's `thread` remains the
actual governing Thread whose admitted signed operation produced that selected
State. Without a grant list, only that governing Thread is allowed. Lists cannot
come from HTTP headers or manifest fields, and the reader still needs a separate
exact view grant. Nested Thread names are bounded logical names, never paths.

Cross-Thread projection follows only exact signed local-integration dependencies
and signed fork-base parent links; unlisted dependencies fail closed. This is a
local-owner closure feature, not hosted/account authorization. A mutable local
`main` ref that fast-forwards to another Thread's accepted State does not create
native admission on main. Such a view must name its actual admitted governing
Thread, and a main-governed request remains denied. The published Git snapshot
may still advertise `refs/heads/main`; that Git projection name does not rename
or grant authority to a native Thread.

Reader policy: `{"readers":[{"sha256":"DIGEST","expires_at":UNIX_SECONDS,
"views":[["repository","source","thread","state",POLICY_EPOCH]]}]}`.
Service policy: `{"services":[{"sha256":"DIGEST","expires_at":UNIX_SECONDS,
"pins":["CATALOG_COMMIT"]}]}`. Empty arrays deny all access. Duplicate JSON keys,
ambiguous credential grants, oversized policy files, malformed digests, unbounded
expiry values and missing files fail closed. The policies remain a bounded MVP
authority adapter, not a replacement for canonical hosted Heddle/Postgres authority.

## Isolation and limits

Native child processes on Linux have a 1 GiB RLIMIT_AS ceiling before parsing
native bytes, 25 CPU seconds, 96 MiB individual-file limit, 128 file descriptors,
disabled core dumps and process-group timeout termination. A local test verifies
the configured address-space limit and rejects a 2 GiB allocation. The host limits
itself to at most 1.5 GiB address space, 256 descriptors and 96 MiB per file, honoring
any stricter inherited hard limits. These are not substitutes for total cgroup
memory, disk and process quotas or request admission controls.

The [container profile](container/README.md) specifies non-root UID/GID 65532,
read-only filesystem and configuration, bounded tmpfs, dropped capabilities,
no-new-privileges, and memory/CPU/PID limits. It has no published ports or baked
credentials, demo files, or fixture harness. Static command-contract tests pass.
No container engine is installed in this environment, so the image build and
actual cgroup/read-only-filesystem enforcement are explicitly **unverified**.

## Repeatable local proof

```sh
export GATEWAY_NATIVE=/absolute/path/to/gateway_native
python3 -m unittest discover -s prototypes/git-gateway -v
node --test prototypes/git-gateway/worker/*.test.mjs
python3 -m unittest discover -s prototypes/git-gateway/container -v
```

`test_host.py` covers actual Linux host startup, both independent authority gates,
policy revocation, ordinary Git clone/fsck, malformed configuration, hard memory
limits, and the Worker→transport adapter→real Python HTTP host→native projection
clone/fetch path. The latter uses `worker/local-host-smoke.mjs` to map an injected
test-only HTTPS origin to loopback HTTP and supplies Artifacts/AUTH response
fixtures. It proves executable local composition, **not TLS or live bindings**.
All credential strings in that harness are conspicuously public test vectors and
must never be provisioned in a deployed environment. The image excludes the harness.

The original synthetic fixture runs two native worker threads with disjoint files;
it is not evidence of two LLM agents or automatic cross-Thread integration. Actual
agent collaboration, hosted source authority, retention semantics, revocation races,
concurrency limits and production operations remain separate verification work.

## Verified local checkpoint (2026-10-02)

- 7 targeted gateway Rust tests pass; full projection crate: 97 unit tests and
  1 publication-boundary integration test pass
- Rust clippy with warnings-as-errors and formatting pass
- 37 Python tests, 36 Worker tests, and 10 container static checks pass
- Actual two-agent app: baseline clone, integrated fetch, fresh clone, strict
  fsck, exact projected OID, byte-identical app.js, all 19 app tests and service
  revocation pass through the local Worker/native-host composition
- The original strict Thread implementation has a retained failing regression;
  the explicit signed-dependency implementation retains wrong-main, unlisted
  dependency, store-only state and signed-privacy denial regressions

The cloud can create user/mount/PID/network namespaces, but `/sys` and a delegated
 cgroup hierarchy are absent; the user systemd scope is unavailable. Installing a
rootless engine alone cannot verify the prescribed container controls here. No
image build/run or sandbox/security-setting change has been performed.

Cloudflare-specific controller configuration is being designed separately. Its
documented instance sizing alone is not evidence of the generic Docker profile's
read-only root, PID, capabilities or no-new-privileges controls. These differences are not user-required Docker flags; they require an explicit
bounded-demo isolation assessment and actual runtime verification. Managed
Cloudflare isolation/resource limits plus non-root execution and authenticated,
immutable synthetic-only input may be a suitable alternative, without claiming
identical filesystem/PID/capability controls. No pending code draft authorizes resource or credential creation.

The Cloudflare-only bootstrap is `gateway.cloudflare_host`, with native Git still
on loopback 8042 and a separately authenticated, exact-route ingress on 8080.
Unlike the generic host, its configuration comes from a strict digest-only
`GATEWAY_DEMO_CONFIG_JSON` snapshot, expiring within 15 minutes. Its fixed internal
bridge uses Cloudflare's documented encrypted same-machine outbound path and
platform Container identity, never a general HTTP fallback or outgoing bearer.
Six Python contracts cover ingress, the bridge, exact policy/config shape and
ordinary local Git clone plus revocation. This is still local code proof, not
image, Cloudflare VM, public TLS, or binding execution proof. The Worker activation
flag, public routes and deployment image reference remain disabled/blocked.
