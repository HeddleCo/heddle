# RustCrypto dependency tracking

Heddle uses AES-GCM 0.11, X25519 Dalek 3 with static secrets, SHA-2 0.11,
HKDF 0.13, Ed25519 Dalek 3, and getrandom 0.4 for its own crypto paths.
The upgrade preserves the golden ciphertext, tags, shared secrets, derived
keys, capability digests, and old at-rest ciphertext in commit `d90f7ce`.

## WASM entropy exception

Tracking: remove the wasm32-only `getrandom = { version = "0.2", features = ["js"] }`
feature enablers from capability-verifier and biscuit-verifier once biscuit-auth
provides a supported JavaScript entropy backend itself. Neither verifier calls
getrandom directly. These dependencies configure biscuit-auth's transitive
getrandom 0.2; Heddle's getrandom 0.4 backend cannot configure a different major.

[biscuit-auth 6's manifest](https://docs.rs/crate/biscuit-auth/6.0.0/source/Cargo.toml)
requires getrandom 0.2, while its `wasm` feature enables wasm-bindgen without
getrandom's `js` feature. This is the documented third-party exception to
removing direct old majors. It is confined to WASM verifier builds.

## Remaining third-party versions

Noq's AES-GCM 0.10 stack, biscuit-auth's older signing, hashing and elliptic-curve
stack, heddle-api's SHA-2 0.10, and transitive rand_core 0.6/0.9 remain outside
this upgrade. Their lower-level RustCrypto versions remain in Cargo.lock until
those upstream dependencies migrate. No forks or compatibility code are added.
