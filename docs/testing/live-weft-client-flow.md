# Live-weft client-flow end-to-end test

`crates/cli/tests/live_weft_client_flow.rs` drives the compiled `heddle`
binary through real push, pull, clone, and server-side public-Git import
operations, then inspects the live weft through the production hosted client. It is `#[ignore]` because weft,
Postgres, object storage, and authentication are external test dependencies.
An unset `HEDDLE_E2E_WEFT_URL` also makes an explicitly selected run skip
cleanly.

Set `HEDDLE_E2E_WEFT_URL` to the authority only, with no spool path. The test
creates a uniquely named project under the authenticated user's personal
spool, then uses the full auto-provisioned URL for the remaining lifecycle.
For example:

```sh
export HEDDLE_E2E_WEFT_URL='https://weft.example.test:443'
export HEDDLE_CREDENTIAL='/absolute/path/to/live-weft-agent.hcred'

cargo test -p heddle-cli --test live_weft_client_flow -- \
  --ignored --nocapture --test-threads=1
```

The import test defaults to `https://github.com/octocat/Hello-World.git`.
Set `HEDDLE_E2E_PUBLIC_GIT_URL` to use another small public repository. It asks
weft to fetch the repository with no provider connection, waits for the durable
operation to complete, clones the resulting spool through Heddle, and compares
all source files and bytes with a shallow Git checkout of the same HEAD.

`crates/cli/tests/import_source_live.rs` is the stricter import proof. It
requires a source large enough to produce at least two durable `RUNNING`
updates and checks that `completed_units` rises before completion. It also
exercises an unreachable public URL and requires the failure to name
`RetryImportSource`. Run it with fresh client homes through:

```sh
export HEDDLE_IMPORT_SOURCE_E2E_SERVER='weft.example.test:443'
export HEDDLE_IMPORT_SOURCE_E2E_DESTINATION_PREFIX='spool/willow-ibis-8e7264'
export HEDDLE_IMPORT_SOURCE_E2E_SOURCE_URL='https://github.com/example/public-repo.git'
export HEDDLE_IMPORT_SOURCE_E2E_SOURCE_DIR='/absolute/path/to/matching-checkout'

cargo test -p heddle-cli --test import_source_live -- \
  --ignored --nocapture --test-threads=1
```

`HEDDLE_CREDENTIAL` is optional when `heddle auth login` already installed a
credential for that authority in the normal Heddle credential store. For a
private bootstrap CA, also set
`HEDDLE_REMOTE_TLS_CA_CERT=/absolute/path/to/ca.pem`. These are the production
client's normal credential and TLS variables; the harness does not hardcode
tokens, passwords, ports, or certificates.

The test asserts:

1. The first `main` push advertises a non-empty `thread_id`, matching managed
   thread metadata and the pushed state.
2. Advancing and pushing `main` again preserves that exact `thread_id` while
   advancing its state. This is the determinism regression assertion.
3. A named thread gets a different non-empty identity without changing
   `main`'s identity or state.
4. Pulling both threads into a fresh initialized repository resolves the same
   hosted identities and materializes the pushed states.
5. Fresh clones selecting `main` and the named thread resolve the same hosted
   identities and materialize the matching files and states.

The live assertion for the first `main` push is expected to remain red until
heddle#1638 lands. Do not weaken it: this harness exists to catch that class of
client/weft integration failure. The auto-provisioned spool is intentionally
left available after the run for server-side diagnosis.
