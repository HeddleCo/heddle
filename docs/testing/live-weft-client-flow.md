# Live-weft client-flow end-to-end test

`crates/cli/tests/live_weft_client_flow.rs` drives the compiled `heddle`
binary through real push, pull, and clone operations, then inspects the live
weft through the production hosted client. It is `#[ignore]` because weft,
Postgres, object storage, and authentication are external test dependencies.
An unset `HEDDLE_E2E_WEFT_URL` also makes an explicitly selected run skip
cleanly.

Set `HEDDLE_E2E_WEFT_URL` to the authority only, with no spool path. The test
creates a uniquely named project under the authenticated user's personal
spool, then uses the full auto-provisioned URL for the remaining lifecycle.
For example:

```sh
export HEDDLE_E2E_WEFT_URL='heddle://weft.example.test:443'
export HEDDLE_CREDENTIAL='/absolute/path/to/live-weft-agent.hcred'

cargo test -p heddle-cli --test live_weft_client_flow -- \
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
