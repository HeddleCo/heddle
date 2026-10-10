> Runtime update: the candidate image now serves through one native Rust
> `gateway_host`, without Python. See [NATIVE-RUST.md](../../NATIVE-RUST.md).
> Current hosted disclosure authority is a required, unconnected integration;
> `/disclosure/<pin>` deliberately fails closed. Earlier Python-specific
> preparation notes below describe the historical reference adapter only.

# Cloudflare gateway: guarded local preparation

This subtree is **not deployable as supplied**. `ACTIVATION_REVIEWED` is a
code-level `false`; public `fetch` and outbound handlers return 503. The Wrangler
file deliberately references a nonexistent `BLOCKED-runtime-not-approved.Dockerfile`,
uses no public route, disables `workers.dev`/preview URLs, and supplies no policy
or credential. No Cloudflare account, namespace, bucket, Container, repository,
secret, image upload, deployment, or live binding has been accessed or created.

The files prepare a **single-reader, one-source, at-most-two-view synthetic demo**.
They are not canonical Heddle/Postgres authority or a production multi-tenant host.
No fixture token, native test identity, native source, catalog or policy is in the
image. Public test vectors exist only in `integration.test.mjs`, never runtime imports.

## Smallest proposed integration

1. The front Worker composes the existing front door with local `AUTH` and `GATEWAY`
   adapters; separate AUTH/GATEWAY Worker resources are unnecessary. It validates
   a pre-provisioned reader bearer against its digest and exact immutable view
   tuple before reading Artifacts. The actual catalog bytes must match the approved
   full manifest, including Git OID, governing Thread, State and policy epoch.
2. `gatewayTransport` strips caller identity/proxy headers, bounds bytes/deadline,
   adds the independent Worker-to-native service bearer, and invokes the one bound
   Durable Object. It never performs a DNS/network fetch to `native-container.invalid`.
3. The `NativeGatewayContainer` Durable Object verifies both authorities and the
   one named instance. It passes only digest/scope configuration at startup and
   proxies to port 8080. Singleflight stays held until the response completes or
   is cancelled; another request receives 429. The native ingress preserves the
   two credentials and rewrites Host to loopback. Native Git remains on 127.0.0.1:8042.
4. The native process independently resolves `/catalog/<exact-pin>` and
   `/native/<logical-source>` through fixed `http://gateway-bindings.internal`.
   Cloudflare's outbound handler checks its **platform-supplied** `ctx.containerId`
   against `NATIVE_CONTAINER.idFromName('approved-native-view-singleton').toString()`
   and checks `ctx.className`, pin/source scopes and expiry before any binding read.
   Caller headers cannot supply this identity. Artifacts metadata is compared again;
   the R2 object is bounded to 8 MiB and its SHA-256 verified before return.
5. `enableInternet=false`; only the exact virtual hostname has an `outboundByHost`
   handler. No `allowedHosts`, general proxy, fallback network `fetch`, raw R2
   credential, Artifacts API credential, or third service secret is used.

The fixed internal HTTP bridge is an **explicit Cloudflare-specific transport**,
not an insecure fallback in the existing HTTPS reader. Cloudflare documents this
channel as same-machine Worker execution, encrypted by its networking stack.
The generic `HTTPSBundleReader` and generic Docker profile remain unchanged.

## Exact draft resources and runtime contracts

`wrangler.draft.json` is schema-valid against the installed official Wrangler
4.146.0 schema. It describes only proposed resources:

| Resource | Name / binding |
|---|---|
| Worker | `heddle-git-gateway-test-20261001-01a0efd3` |
| Container application | `heddle-git-native-test-20261001-01a0efd3` |
| Container class / DO binding | `NativeGatewayContainer` / `NATIVE_CONTAINER` |
| Named instance | `approved-native-view-singleton` |
| R2 bucket / binding | `heddle-git-native-test-20261001-01a0efd3` / `NATIVE_BUNDLES` |
| Artifacts namespace / binding | `heddle-git-test-20261001-01a0efd3` / `ARTIFACTS` |
| Artifacts repository | `native-view-01a0efd3` |
| R2 object key | `native/<approved-bundle-sha256>.bundle` |

The Container uses **default** scheduling with SQLite DO storage (`exports`),
`basic` size, maximum one active instance, SSH disabled, and optional logs disabled.
`@cloudflare/containers` does not support the distinct `durable_object` scheduling
policy. The separate `Dockerfile` here is the future image candidate with repository
root build context, runtime UID/GID 65532, root-owned application files, and only the
new Cloudflare bootstrap/bridge modules added. It has not been built or run.

Two independently provisioned high-entropy, short-lived bearers are needed:

- Reader bearer: used by the Git client; only its SHA-256 digest appears in policy
- Worker-to-native service bearer: `GATEWAY_SERVICE_CREDENTIAL` Worker secret;
  its digest must match policy; only the digest is passed in startup configuration

The policy never mints, renews or provisions secrets. Supplying missing, empty,
malformed, expired or out-of-scope configuration denies all affected requests.
No sample credential or complete working authority file is provided.

`GATEWAY_POLICY` is trusted deployment configuration: sorted-key canonical JSON,
compact encoding with one trailing newline, at most 64 KiB. Duplicate JSON fields,
unknown fields, unsafe integers, ambiguous grants, mismatching manifest/source
scope and cross-role digest reuse are rejected. Its exact keys are:

- `schema: 1`, `catalog` (must equal `CATALOG_REPO`)
- `issued_at`, `expires_at`: Unix seconds; validity at most **900 seconds**
- `pins`: one/two catalog commit → complete canonical manifest objects
- `readers`: exactly one `{sha256, expires_at, views}`; views are exact
  `[repository, source, thread, state, policy_epoch]` tuples for the approved pins
- `gateway_service`: `{sha256, expires_at, pins}` for those exact approved pins
- `bridge`: `{expires_at, pins, sources}` for the exact approved pins and one source
- `sources`: one logical name → `{key, sha256, authorized_threads}`; the R2 key is
  content-addressed, Thread names are explicit and include the governing Thread

All grant expiries are inside the policy window. For the actual-agent demo the
integrated governing Thread is `demo/priority-sort`; explicit dependencies are
`main` and `demo/open-filter`. A projected Git `refs/heads/main` is not native
admission on main. Use the fresh native-v6 dataset's `DATA.json` for its bundle
digest, immutable catalog pins and projected Git OIDs; see
[`CURRENT-RUN.md`](../../CURRENT-RUN.md) for the current rehearsal. October 2
native-v5 pins are historical and incompatible with the current runtime. A local
dataset and its pins are not evidence of upload or published hosted resources.

`nativeConfig()` derives `GATEWAY_DEMO_CONFIG_JSON`, containing exactly:
`schema`, `expires_at` (earliest grant expiry), sorted `published_pins`,
`reader_sha256`, `service_sha256`, `views` (in pin order), `descriptors`
(source → SHA-256), and `authorized_threads` (source → list). The native entrypoint
is `/usr/local/bin/gateway_host`; ingress and bridge origins are fixed in code.
No arbitrary file path, command, URL or raw secret is accepted from clients.

## Isolation choice and residual risks

Cloudflare documents per-Container Linux VM execution and finite resource sizing.
`basic` provides **1 GiB memory, 1/4 vCPU, 4 GB ephemeral disk**. Cloudflare reports
OOM restart and no swap. The existing native Linux limits additionally bound
address space (1 GiB per child), CPU time, open files, per-file size and execution
deadlines. Non-root execution, root-owned application files, one request at a time,
fixed immutable synthetic source, exact grants, and a 15-minute authorization
window form a credible bounded demo profile, subject to actual runtime testing.

This is **not the generic Docker hardening profile**: Cloudflare's documented
Wrangler surface does not establish a read-only root filesystem, a 64-PID cgroup,
dropped capabilities, no-new-privileges, or the 512 MiB noexec tmpfs. We do not claim
those controls. Writable temporary/config files and total disk have the platform
instance limit; per-file/address-space limits are not equivalent total quotas.
Performance under 1/4 vCPU, peak memory, untrusted-input behavior, ingress reachability,
actual cgroup behavior and egress enforcement remain unverified. Do not use private
or customer content, arbitrary bundles, or unrelated workloads for this demo.

A 1-vCPU/1-GiB custom instance is **invalid** under current documented constraints:
custom instances require at least 3 GiB per vCPU. If `basic` cannot complete this
bounded synthetic workload within its deadline, the choice is a reviewed 1-vCPU/
3-GiB profile or a different host retaining the generic controls, not silently
relaxing limits. Native generation remains bounded and failed requests fail closed.

## Expiry, configuration changes and activation choices

Every edge/bridge request re-reads the active Worker configuration. Controller and
native host independently check expiration on each request; bridge responses are
also checked after storage reads. The Linux policy is a startup snapshot, not a
live authority database. Deployed Worker versions/config changes can propagate at
different times. Immediate global revocation is not claimed. In-flight requests
may have been admitted before revocation; no post-revocation byte-erasure promise
is made.

A controller with a running Container and unknown/different startup config rejects
requests; it does not silently reuse stale native policy. Stop/restart is needed
after configuration changes or DO reconstruction that loses the in-memory config
marker. Idle timeout is 30 seconds; hard authorization expiration remains 15 minutes
at most. This conservative marker can cause availability failures; it is not a
production rollout mechanism.

Before changing the code guard or missing-image reference, the operator needs to
choose the bounded Cloudflare profile knowingly, verify the candidate image and
actual ingress/private-bridge behavior, and approve the exact synthetic pins,
scopes, expiry and external credential provisioning. Resource creation, publication,
upload, secret installation and deployment each remain outside this local work.
This file is not an approval request or a statement of readiness to deploy.

## Local verification only

```sh
# Public npm packages pinned in package-lock.json; no account credentials needed.
npm ci --ignore-scripts --no-audit --no-fund
npm test
npm run check
npm run bundle
```

`npm run bundle` invokes esbuild directly; it does not invoke Wrangler/Docker,
read Cloudflare credentials, create resources, upload anything or build an image.
The root prototype's original Worker tests should also continue to pass.
Wrangler JSON-schema validation is local; it does not establish backend acceptance
or resource existence. No `wrangler deploy` or remote dev command was run.

The tests use injected Artifacts/R2/DO shapes and public test vectors. They cover
fail-closed policy, exact pin/source scope, expiry before/after awaits, both
independent credentials, spoofed identity/proxy headers, byte/digest mismatch,
capability disposal, platform-ID scoping, native startup schema, singleflight
through stream completion/cancellation, and stale native configuration rejection.
They are not Cloudflare runtime/TLS/VM enforcement tests.

## Primary references checked 2026-10-02

- [Container class](https://developers.cloudflare.com/containers/api/container-class/):
  DO controller model, Linux VM, ports, lifecycle, envVars, default scheduling only
- [Outbound traffic](https://developers.cloudflare.com/containers/configuration/outbound-traffic/):
  exact-host handler, trusted context, internet-deny model, managed HTTP encryption
- [Workers bindings from Containers](https://developers.cloudflare.com/containers/configuration/workers-connections/):
  virtual hostname bridge to R2 and other bindings
- [Environment and secrets](https://developers.cloudflare.com/containers/examples/env-vars-and-secrets/):
  explicit Worker-to-Container startup configuration
- [Durable Object IDs](https://developers.cloudflare.com/durable-objects/api/id/):
  namespace IDs and toString; obtaining an ID alone creates no object
- [Limits](https://developers.cloudflare.com/containers/platform/limits/) and
  [FAQ](https://developers.cloudflare.com/containers/faq/): sizing, ephemeral disk,
  no swap, OOM and lifecycle caveats
- [Wrangler configuration](https://developers.cloudflare.com/workers/wrangler/configuration/):
  Container names/bindings/build context, exports and secrets.required
- [Official package source](https://github.com/cloudflare/containers): installed
  version 0.3.7 passes `this.ctx.id.toString()` and constructor name as trusted
  ContainerProxy props (dist/lib/container.js), not client-supplied headers
