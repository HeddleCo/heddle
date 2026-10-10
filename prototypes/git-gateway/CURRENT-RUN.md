> Preserved historical 0.29.0 demo checkpoint. Current Heddle 0.30.0 hosted authentication, receiver acceptance and publication code is described in [HOSTED-PUBLICATION.md](HOSTED-PUBLICATION.md) and [HOSTED-RUNTIME.md](HOSTED-RUNTIME.md). Statements below about missing hosted interfaces apply to this earlier path, not the current implementation.

# Current Heddle / thin Cloudflare rehearsal

The latest Cloudflare candidate uses a Python-free Rust serving path with bounded
projection caching and mandatory current disclosure checks. See
[`NATIVE-RUST.md`](NATIVE-RUST.md). The Python commands below remain reference
fixture/compatibility checks, not the new Cloudflare runtime. Hosted disclosure
authority is not connected; the bridge fails closed.

This local update is based on Heddle **0.29.0**, commit
`49cdd66aea09d53c6c02e8266cee2f727d078937`, with the recovered prototype applied.
It preserves a small manifest-only Artifacts catalog, Worker front door, and
native reconstruction into a fresh, request-private Git repository. No complete
source Git mirror is stored in Artifacts. The pinned Cloudflare path is read-only.
The newer [local Git window](GIT-WINDOW.md) provides ordinary push and complete
supported history on one existing local public Thread, with a filesystem
publication adapter. Hosted write authority and Artifacts/R2 write publication
are separate outstanding integrations.
The repository's Apache-2.0 license covers these source additions.

## One-command local verification

Install Rust 1.98.0, Git including `git-http-backend`, Python 3.11+, and Node 22+.
Install the locked public Worker build dependencies once:

```sh
(cd prototypes/git-gateway/worker/cloudflare && npm ci --ignore-scripts --no-audit --no-fund)
prototypes/git-gateway/verify-local.sh /absolute/new/evidence-directory
```

The script builds the examples, runs projection tests, clippy, formatting,
Python/native integration tests, the authorization mutation check, packaging
contracts, Worker tests, and Worker bundle generation. It then leaves two Git
clones and a machine-readable local smoke report. It never invokes Wrangler,
Docker, cloud account APIs, deployments, or credential provisioning.

For only the end-to-end local demonstration:

```sh
cargo build --locked -p heddle-git-projection --examples
export GATEWAY_NATIVE="$PWD/target/debug/examples/gateway_native"
export PYTHONPATH="$PWD/prototypes/git-gateway"
python3 -m gateway.smoke --binary "$GATEWAY_NATIVE" --out /absolute/new/smoke
```

The flow uses the actual Worker router and transport through an explicitly
loopback-mapped test transport to the actual native HTTP gateway and Git
`upload-pack`. Artifacts and R2 callbacks are local fixtures. It verifies base
clone, updated fetch, independent fresh clone, exact projected OIDs, strict
fsck, no-credential denial, and independent reader/service revocation before
native bytes. Each HTTP request extracts native data and reconstructs new Git
objects. The default generated fixture deletes its source worktrees before
serving. The report explicitly records `live_cloudflare_verified: false`.

## Actual coding-agent rehearsal

`gateway_agent_demo` is a narrow, local demonstration helper built against real
Heddle APIs. It is **not** the full CLI's `ready`, `resolve`, or `land` flow and it
contains no automatic merge algorithm. Two actual coding agents must perform
the parallel implementation work; a test harness cannot establish that fact.

1. Copy the public toy baseline into a new `RUN/native` directory.
2. Set `HEDDLE_HOME=RUN/local-demo-identity` and a synthetic principal. Set true
   agent attribution, including the actual task/session label. Where the runtime
   does not expose the exact model, use the explicit value
   `runtime-model-not-disclosed`, and disclose that limitation in the evidence.
3. Run `gateway_agent_demo prepare RUN`. It creates native v6 baseline and
   `RUN/agent-filter` / `RUN/agent-sort` checkouts on separate signed Threads.
4. Start two actual coding agents concurrently. Record independent start/end
   times, failing-before/passing-after tests, changes and invariant notes.
5. Each uses `gateway_agent_demo capture RUN THREAD` with its own actual
   attribution environment, then `gateway_agent_demo annotate RUN THREAD NOTE`.
   Thread is `demo/open-filter` or `demo/priority-sort`. Annotation text is
   bounded to 8 KiB and stored as a native context attachment for `app.js`.
6. Review and explicitly compose both changes in a separate directory. Run the
   combined behavior tests. Preserve a real textual conflict if one occurs;
   do not call manual composition an automatic Heddle conflict resolution.
7. Run `gateway_agent_demo integrate-reviewed RUN COMPOSED_DIRECTORY` as the
   coordinator. It creates a true two-parent native State and admitted signed
   LocalIntegration, unions agent context, and appends coordinator context.
   The governing Thread is `demo/priority-sort`; native `main` remains unchanged.
8. Run `gateway_agent_demo inspect RUN STATE` to verify parents, native context
   and recorded attribution. The coordinator's annotation must stay labeled as
   coordinator-authored. The example's signing identity is local and synthetic;
   it is not an independently enrolled hosted identity for each agent.

Pack the exact fresh native states into an identity-free local dataset:

```sh
python3 -m gateway.pack_demo --synthetic --binary "$GATEWAY_NATIVE" \
  --native RUN/native --base BASE_STATE --integrated INTEGRATED_STATE \
  --authorized-thread main --authorized-thread demo/open-filter \
  --authorized-thread demo/priority-sort --out /absolute/new/dataset
python3 -m gateway.smoke --binary "$GATEWAY_NATIVE" \
  --dataset /absolute/new/dataset --out /absolute/new/agent-smoke
```

The packing command validates real signed native projection before writing a
local Git metadata catalog and SHA-256-addressed native bundle. It performs no
remote publication. Keep the original rehearsal directory private: it contains
a synthetic signing identity. Never package the whole run directory or caches.
The resulting transport data is permitted only for synthetic demo use.

## Repository-format cutover

The old October 2 actual-agent archive is **v5** and must not be reused with the
current **v6** runtime or its old catalog pins. Current Heddle intentionally has
no converter or open-time fallback. Never change `repository.version` by hand.
The smoke CLI checks the bundled format against `gateway_native format` and
refuses mismatches before opening a listener. Rebuild from the public baseline
with two fresh actual agents. The old archive remains historical evidence only.
See [upstream v6 instructions](../../docs/migrations/ref-names-v6.md).

## Remaining live acceptance gates

The Cloudflare source is prepared and guarded; its activation flag remains
false, the draft image reference is deliberately blocked, and routes remain
disabled. Local verification is not cloud deployment evidence.

After separately approved live scope and prerequisites:

- Build and inspect the AMD64 Container image in a suitable engine/runtime.
  Static Docker command tests do not prove cgroup, filesystem or network isolation.
- Choose the exact account, namespace/catalog, R2 object digest/key, two new
  catalog pins, expiry, service/read identities, resource limits and budget.
- Use secure supported credential provisioning; never embed or print bearer
  values, signing identities or account credentials in source/evidence/video.
- Publish only the local metadata catalog to Artifacts and the synthetic native
  bundle to R2, with separately authorized resource and permission changes.
- Explicitly review activation and the approved config, then deploy. No step in
  this document authorizes those actions.
- Through the real deployed Worker, establish successful ordinary Git
  clone/fetch/fresh clone and fsck, exact OID/content equality, native-only cold
  reconstruction, authenticated denial, both revocation paths, bounded expiry,
  blocked receive-pack, actual private bridge/egress isolation and cost/runtime.
- Record exact resource names, run times, pins, bundle digest, observed requests
  and cleanup outcome without secrets. Record which checks used live services.

The runnable source changes are still local until explicitly published. A
permissive license on the existing repository does not make unpublished changes
available to competition judges. Source publication, a new 5–10-minute video,
and the user's manual contest submission remain separate completion gates.
