# Owner-authz v2 conformance fixtures

The published crate embeds three canonical-protobuf JSON fixture sets:

- `fixtures/v2.json`: a valid purge and ten named negative decisions;
- `fixtures/transfer-v2.json`: complete and incomplete two-owner handoffs; and
- `fixtures/keyring-v2.json`: a self-rooted linear keyring and a fork.

Every protobuf value is lower-case hex of the exact encoded bytes. Fixed UUIDs,
state hashes, and payloads are also hex. Adapters decode and re-encode
byte-for-byte before verification so unknown fields, aliases, duplicates, and
trailing bytes fail closed.

```rust
use heddleco_capability_verifier::conformance::{
    FIXTURE_V2_JSON, KEYRING_FIXTURE_V2_JSON, TRANSFER_FIXTURE_V2_JSON,
    run_fixture, run_keyring_fixture, run_transfer_fixture,
};

assert!(run_fixture(FIXTURE_V2_JSON)?.iter().all(|case| case.matches));
assert!(run_transfer_fixture(TRANSFER_FIXTURE_V2_JSON)?
    .iter().all(|case| case.matches));
assert!(run_keyring_fixture(KEYRING_FIXTURE_V2_JSON)?
    .iter().all(|case| case.matches));
# Ok::<(), heddleco_capability_verifier::Error>(())
```

All adapters are pure and execute from the same source on native Rust and
`wasm32-unknown-unknown`. Fixture regeneration is a maintainer-only ignored
test; the production library exposes no signing or private-key API.
