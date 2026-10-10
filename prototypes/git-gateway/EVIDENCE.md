> Historical evidence below was recovered from the October 2 checkpoint. For fresh October 10 verification on Heddle 0.29.0 / format v6, see CURRENT-RUN.md and the packaged fresh-test logs. Old v5 native bundles are not current runtime inputs.

# Initial local verification and review summary

The subsequent adversarial review, adapter implementation, and latest results are in
[AUDIT.md](AUDIT.md). The logs below preserve the initial implementation checkpoint.

Base: `ecf70cc15c9f7ffc620345ea0a8d8c007ab2eb7c` (official public Heddle).
See [PROVENANCE.json](PROVENANCE.json) for exact locked dependencies and tools.

## Results actually executed

| Check | Result | Evidence |
| --- | --- | --- |
| Targeted Rust native projection tests | 3 passed | [rust-tests.txt](evidence/rust-tests.txt) |
| Ordinary Git HTTP + native source/catalog integration | 8 passed | [gateway-tests.txt](evidence/gateway-tests.txt) |
| Worker binding/front-door contract with test doubles | 6 passed | [worker-tests.txt](evidence/worker-tests.txt) |
| Child deadline and hard 96 MiB output-file limit | 2 passed | [process-tests.txt](evidence/process-tests.txt) |
| Authorization mutation | Expected failure detected: denied request became HTTP 200 | [authorization-mutation.txt](evidence/authorization-mutation.txt) |

The native fixture creates two isolated signed native Threads concurrently. The Git tests
clone the base snapshot, fetch a derived view, compare both workers' file contents, run
`git fsck --strict`, and exercise protocol versions 1 and 2. They verify deterministic
reprojection, a separate native merge's ordered parents, inaccessible/unserved objects,
request/path/write denial, revocation and stale epochs, source/OID mismatch, missing native
state, corrupted config and actual native packs, publication CAS races, and replay after a
lost acknowledgment. Source file hashes are compared before/after serving; the authoritative
fixture is unchanged. A signed original's entry privacy still blocks export after its mutable
sidecar is removed. Rust state/entry/byte/blob limits are exercised with zero budgets.

An observed pair proving catalog/source separation in the final HTTP test:

* Manifest catalog commit: `77f14c3c2b201a8358aa3423cf86e5f60505cc0f`
* Projected source Git commit: `794fc88b67da6d6cac063b49603b3c6c6e0e24cc`
* Native state: `hs-edjpxk68k7axbd7dabxys0pb0k8fhp4cgx89b77d1hn0yg18txkg`

These IDs belong to the test's now-cleaned temporary fixture. The retained demo's actual IDs
are in `.demo/local/evidence.json`; that ignored directory includes synthetic local signing
identities and must not be added to source or a patch.

## Commands used on this Mac

From `<historical-workspace>`:

```sh
CARGO_HOME="$PWD/.cargo-local" CARGO_TARGET_DIR="$PWD/target" cargo build --manifest-path heddle-current/Cargo.toml -p heddle-git-projection --example gateway_native --locked --offline
CARGO_HOME="$PWD/.cargo-local" CARGO_TARGET_DIR="$PWD/target" HEDDLE_HOME="$PWD/rust-test-home" cargo test --manifest-path heddle-current/Cargo.toml -p heddle-git-projection gateway_view --locked --offline
GATEWAY_NATIVE="$PWD/target/debug/examples/gateway_native" python3 -m unittest discover -s heddle-current/prototypes/git-gateway -v
node --test heddle-current/prototypes/git-gateway/worker/index.test.mjs
GATEWAY_NATIVE="$PWD/target/debug/examples/gateway_native" python3 heddle-current/prototypes/git-gateway/check_authorization_guard.py
```

The final Python coverage was run as the 8 gateway tests and the 2 process tests separately;
the discover command above runs both. Initial dependency download used the same cargo build
without `--offline`. The Mac sandbox required permission for public source/dependency reads
and the loopback listener. Those actions succeeded. No access blocker remains for local tests.

## Reviewable change scope

* Existing file: `crates/git-projection/src/lib.rs` adds one public module declaration.
* New `gateway_view.rs`: bounded, admitted, signature-verified all-public native projection.
* New `examples/gateway_native.rs`: fixture generator and local projection adapter.
* New `prototypes/git-gateway/gateway/`: local Git catalog, resolver/authorization,
  native materialization, process limits, HTTP and retained demo setup.
* New `worker/`: undeployed Worker, Artifacts read interface, binding contract tests/config.
* New Python tests, mutation check, provenance, instructions and this evidence report.

No existing exporter, storage backend, CLI verbs, Cargo dependencies or Cargo.lock were changed.
The original developer checkout remains clean. Installed `heddle` still reports 0.15.0;
the prototype uses only its isolated build in `task/target`.

## Limits and unverified work

No live account validation, deployment, external push, PR, hosted SQL, cloud configuration
mutation or hosted credentials. Worker tests are test-double contract checks, not live
Artifacts validation. Fixture headers are not authentication. R2/Postgres/Weft adapters,
real identity services and service-binding wiring are absent. The combined Thread output
is an explicit derived capture; it is not a cross-Thread merge implementation. Imported
Git fidelity, partial views and broader history support remain rejected. OS memory quotas,
production admission and retention transactions are not implemented. Competition eligibility
is not established. See [README.md](README.md) for exact boundaries and approval-required
live validation steps.
