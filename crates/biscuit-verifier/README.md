# heddle-biscuit-verifier

Shared ordinary Biscuit access, attenuation, resource-path, and grant-envelope
verification. Extracted without rule changes from Weft's
`crates/weft-capability-verifier` at `1bf573d5`. It has no repository, daemon,
transport, or database dependency.

Hosts supply the trusted roots, resource, operation, and evaluation time using
`verify_any_at_with_resource`. Hosts also enforce account attachment, persisted
revocation, request proof/replay protection, and resource policy. The convenience
functions that use the clock are optional callers of that same verification core.

Owner authority, recovery, transfer, and purge use the adjacent
`heddleco-capability-verifier`. Ordinary spool write/admin capabilities do not
substitute for owner purge authorization.

`delegation::AgentAttenuation` constructs the shared agent restriction block.
`time_bounded` preserves the parent's scope; explicit operation/resource lists
narrow it, and an empty list denies work. The caller adds the signed child-key
binding before appending the block. Key delegation alone preserves the human's
account authority; a self-observation exception through explicit work ceilings
still returns only the limited self-view. Hosts continue to enforce every
ancestor caveat and revocation dependency.

Run from the Heddle workspace:

```sh
cargo test --locked -p heddle-biscuit-verifier --all-targets
cargo clippy --locked -p heddle-biscuit-verifier --all-targets -- -D warnings
```
