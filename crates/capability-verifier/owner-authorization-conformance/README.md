# Owner-authorization differential conformance

This repository owns the executable parity gate for the canonical verifier.
The harness starts from every checked-in `conformance/fixtures/*` matrix,
applies deterministic evidence mutations, and sends the identical corpus to:

1. a native adapter whose dependency is the repository root by path; and
2. the publishable `@heddleco/capability-verifier-wasm` WebAssembly package.

The comparison is deliberately native Rust versus WebAssembly built from the
same source. Tapestry can consume the WebAssembly package instead of carrying
a second authorization implementation, so this gate checks target/binding
parity without allowing the two implementations to drift.

Install `wasm-pack`, then run one seed with:

```bash
OWNER_AUTH_CASE_SEED=38322398 \
  owner-authorization-conformance/run.sh
```

`OWNER_AUTH_FUZZ_CASE_COUNT` changes the number of mutations made per fixture.
CI runs all four seeds in `seeds.txt`. Each corpus is retained below
`target/owner-authorization-conformance/corpus` for exact local replay.

Set `OWNER_AUTH_FORCE_DIVERGENCE=1` only when testing the gate itself. It
injects a result mismatch after both verifiers run and must make the harness
fail with `OWNER_AUTH_DIFFERENTIAL_DIVERGENCE=DETECTED`.
