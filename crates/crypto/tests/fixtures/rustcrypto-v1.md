These golden bytes were captured by running Heddle at `9f0753be`, before
changing dependencies: AES-GCM 0.10.3, X25519 Dalek 2.0.1, HKDF 0.13.0,
and capability-verifier SHA-2 0.10.9. All keys and payloads are synthetic.

`rustcrypto-v1.json` pins fixed-input AES-256-GCM ciphertext and tag,
both X25519 public keys and their shared secret, the production HKDF wrap
key (including its salt ordering and domain), the wrapped DEK, and the
length-prefixed padded ciphertext. The binary copies feed env-store's
persisted MessagePack fixtures, whose old bytes must decode and decrypt.
Biscuit-verifier separately pins a sealed signature-v1 credential.
Capability-verifier pins every local SHA-256 domain, key ids, policy hashes,
genesis and payload digests, and WebAuthn RP/client-data hashes in a test
that runs on both native Rust and WASM. Existing conformance fixtures also
exercise the complete authorization and signature paths.

Capture verification (using the unchanged lockfile):

```sh
cargo test --locked -p heddle-crypto -p heddle-env-store \
  -p heddleco-capability-verifier -p heddle-biscuit-verifier \
  rustcrypto_ -- --nocapture
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
  cargo test --locked -p heddleco-capability-verifier \
  --target wasm32-unknown-unknown --lib -- --nocapture
```

All four native golden tests passed; all 74 WASM tests passed, including
the new digest vectors, using wasm-bindgen-test-runner 0.2.127.
Keep these fixtures unchanged when upgrading dependencies.
