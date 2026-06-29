# heddle#545 — FIDO2/CTAP2 Rust library evaluation for CLI hardware-key signing

**Status:** spike deliverable (decision doc). **Decides:** which Rust FIDO2/CTAP2
library — if any — heddle's planned `HardwareSigner` should be built on, for an
on-token hardware credential the CLI signs identity/state assertions with.
**Split out from:** #38 (hardware-key-backed CLI identity), which bundled this
*evaluation* with the CLI *wiring*. #38 is now impl-only, blocked by this spike.
**Relates to:** weft#183 (hardware-key recovery).

All heddle file:line citations are at the branch base `origin/main` (`7557a1cb`).
Crate versions/dates were verified against crates.io / docs.rs / lib.rs in
June 2026 (see §6 for the exact figures and the citations).

---

## 1. Verdict

**Recommend: DEFER hardware-key signing past v0.3.0; do NOT add a FIDO2 dependency
yet. When it is built, build it on `ctap-hid-fido2` (pure-Rust CTAP2-over-HID),
behind a non-default `hardware-key` Cargo feature, with a *new* `assertion`
signature algorithm — NOT by squeezing a FIDO2 assertion through the existing
`Signer::sign(&[u8]) -> Vec<u8>` trait.**

Two findings drive the defer, and they are independent:

1. **The trait shape is wrong, not just the library choice.** A FIDO2/CTAP2
   *assertion* does not sign the bytes you hand it. It signs
   `authenticatorData || SHA-256(clientDataHash)`, where the only caller-controlled
   input is a 32-byte *challenge* embedded in the client-data hash. The token
   returns the signature **plus** the `authenticatorData` it minted, and the
   verifier needs both. heddle's `Signer` trait (`crates/crypto/src/lib.rs:31-36`)
   is `sign(&self, data: &[u8]) -> Result<Vec<u8>, SignerError>` and its verify
   path `verify_payload_signature` (`crates/crypto/src/lib.rs:102-114`) does a raw
   `Ed25519Signer::verify_with_public_key(payload, public_key, signature)` over the
   exact payload bytes. **A hardware assertion cannot satisfy that contract** — the
   bytes actually signed differ from `payload`, and the extra `authenticatorData`
   has nowhere to live. This is a *format/verification* change (a new algorithm
   discriminator in the on-disk signature, plus a new verify branch), which is
   exactly the schema-stability surface heddle#451 governs. It is not a drop-in.

2. **The cost/benefit doesn't clear the bar today.** Per the
   [[cli-ergonomics-over-feature-count]] lens, a feature must be ergonomic *and*
   justified, not added for completeness. Hardware-key signing adds: a USB-HID
   dependency (a C `hidapi` backend or the libfido2 C lib), Linux udev-rule
   provisioning friction (unprivileged HID access needs a `udev` rule installed),
   button-tap/PIN UX that must not hang on unplug, a new on-disk signature
   algorithm with its own migration story, and a cross-platform test burden that
   needs *physical tokens in CI*. heddle's current identity story — software
   Ed25519 device keys + biscuit attenuation (`docs/SELF_SOVEREIGN_AUTH.md`,
   `crates/client/src/device_flow.rs`) — already gives self-sovereign,
   non-exfiltratable-by-network keys at `0600` on disk
   (`crates/crypto/src/lib.rs:70-81` enforces the perm floor). The marginal threat
   closed by hardware keys (local disk theft of the device key) is real but narrow,
   and the [[cli-ergonomics-over-feature-count]] question — "is this worth the
   permanent UX + maintenance tax on *every* heddle build that links it?" — answers
   *not yet* at v0.3.0.

**If/when #38 is greenlit**, `ctap-hid-fido2` is the right substrate: it is
pure-Rust CTAP2 (no Yubico C lib), actively maintained, exposes the raw
`make_credential` / `get_assertion` primitives a CLI identity needs, and avoids the
WebAuthn relying-party ceremony shape. The only C dependency it pulls is `hidapi`
(small, native-backend, ubiquitous) — far below the libfido2-FFI burden.

---

## 2. Use-case framing — what a CLI hardware key must actually do

heddle's identity is an **Ed25519** keypair. The signer abstraction
(`crates/crypto/src/lib.rs:31-36`) backs three software algorithms today —
`ed25519`, `rsa`, `p256` (`load_signer`, lib.rs:41-60) — and the auto-mint identity
path (#482) loads a software signer from `identity.toml` to sign state objects.

A hardware-key identity wants the **same abstraction**, but with the private key
living on a FIDO2 token (the `ssh-keygen -t ed25519-sk -O resident` /
`age-plugin-yubikey` model): the credential is provisioned once
(`make_credential` → a resident credential, optionally discoverable so it survives
a fresh machine), and each signing operation is a CTAP2 `get_assertion` that
requires user-presence (a button tap) and optionally a PIN. Private material never
leaves the token.

The hard constraint that shapes everything below:

> **CTAP2 assertions are not raw signatures over caller bytes.** `get_assertion`
> signs `authenticatorData || SHA-256(clientDataHash)`. The 32-byte challenge inside
> `clientDataHash` is the only input the caller controls; `authenticatorData`
> (containing the RP-ID hash, a user-presence flag, and a monotonic signature
> counter) is minted by the token and returned alongside the signature. Both are
> required to verify. So "sign this state-content-hash with the hardware key" maps
> to "use the 32-byte content hash as the CTAP2 challenge, and persist the returned
> `authenticatorData` next to the signature."

This is why §1 finding (1) holds: the heddle `Signer::sign` → `Vec<u8>` contract has
no slot for the returned `authenticatorData`, and `verify_payload_signature` would
have to reconstruct `authenticatorData || SHA-256(challenge)` rather than verify over
the raw payload. A hardware signer is a **new signature algorithm** (call it
`fido2-assertion`), not a fourth backend behind the existing identical-shaped trait.

### Ed25519-on-token caveat

Ed25519 resident credentials require a reasonably modern token (e.g. YubiKey
firmware ≥ 5.2.3; older tokens only do ECDSA P-256 / `es256`). The credential is
created by requesting the **EdDSA** COSE algorithm (`-8`) in `make_credential`;
tokens that lack it fall back to `es256` (`-7`). heddle's identity being Ed25519
means a hardware path should prefer EdDSA and *refuse-with-advice* on a token that
can't do it (rather than silently minting a P-256 credential under an "ed25519"
identity). All three candidate libs can request a specific COSE alg in
`make_credential`; none *guarantees* the token honors EdDSA — that's a runtime
capability check, not a library-selection axis.

### How this relates to the existing software device key (biscuit `cnf` / proof key)

heddle already uses a software **Ed25519** key as the self-sovereign identity, and
biscuit attenuation (`crates/client/src/device_flow.rs`,
`docs/SELF_SOVEREIGN_AUTH.md`) for delegated agent auth. A hardware key would
**augment, not replace**, that model in the near term:

- The biscuit `cnf` / proof-of-possession key is a *device-binding* key used in the
  attenuation/agent-delegation flow. It is short-lived-ish, machine-local, and its
  job is "prove this request comes from the device that holds the parent token."
- The state-*signing* identity (the thing #38/#545 target) is the durable authorship
  key. *This* is the one worth putting on hardware: it's the long-lived,
  high-value, "did this human/agent actually author this state" key.

The clean design is: **hardware key = the durable signing identity; software
Ed25519 = the device-PoP/biscuit `cnf` key stays software** (it's per-device and
ephemeral enough that token-tapping every request is the wrong UX). Replacing the
`cnf` key with hardware would force a button-tap on every delegated call — a UX
non-starter. So hardware-key work is scoped to the *authorship/state-signing*
identity only.

---

## 3. Candidate matrix

| Axis | `fido2-rs` (+ `libfido2-sys`) | `ctap-hid-fido2` | `webauthn-authenticator-rs` |
|---|---|---|---|
| **What it is** | Safe Rust wrapper over Yubico's **libfido2 C** library | **Pure-Rust** CTAP 2.0/2.1 over USB-HID (`hidapi`) | Client/authenticator half of `webauthn-rs`; CTAP 2.0/2.1 over multiple transports |
| **Raw CTAP2 sign?** | ✅ exposes `make_credential` / `get_assertion` (libfido2 `fido_assert_*`) | ✅ exposes `make_credential` / `get_assertion` directly | ✅ exposes `MakeCredentialRequest` / `GetAssertionRequest` (NOT only RP-ceremony shaped) |
| **Ceremony shape** | low-level, CLI-friendly | low-level, CLI-friendly (ships a `ctapcli` example tool) | client-of-a-browser shape, but the CTAP2 request structs are reachable directly |
| **C / system deps** | **Heavy**: builds libfido2 from source on Linux/macOS (needs C compiler, cmake, OpenSSL/libcbor/zlib via pkg-config); MSVC pulls a prebuilt DLL | **Light**: pure-Rust *logic*; transport is `hidapi` (small C lib w/ native backends), crypto via `ring`/`aws-lc-rs` | Medium: `usb` feature pulls a HID stack; optional `nfc` (PC/SC), `bluetooth`, caBLE, Win10 WebAuthn API |
| **Transports** | USB + NFC | **USB-HID only** | USB-HID, NFC, BLE, caBLE/hybrid, Win10 native |
| **Platform coverage** | Linux/macOS (build-from-source), Windows (DLL) | macOS, Windows (admin), Linux/RPi (libusb+libudev). udev rule needed for unprivileged Linux HID | broadest (USB/NFC/BLE/Win10), but maturity uneven across transports |
| **Maturity / health** | v0.5.0 (Apr 2026), Rust-2024; small maintainer (tyan-boot); thin wrapper, so health ≈ libfido2's (very healthy upstream) | v3.5.11 (May 2026); actively maintained; mature API, `ctapcli` proves the flows | v0.5.5; **self-described pre-1.0 / "alpha", no security review, no FIDO certification, API unstable** |
| **Fit w/ heddle no-heavy-FFI lean** | ✗ pulls a full C build toolchain into every build that links it | ✓ best fit — only `hidapi` C backend, no Yubico C lib | ~ heavier transport stack; alpha status is the bigger concern than FFI |
| **Risk** | FFI build fragility across distros; supply-chain surface of a C build | pure-Rust maturity risk (smaller ecosystem), USB-HID-only (no NFC/BLE) | API churn + un-reviewed crypto in the signing path is a hard no for a security substrate |

### Per-candidate assessment notes

**`fido2-rs` / `libfido2-sys`.** Does exactly the right CTAP2 operations and rides
Yubico's reference C library — the most battle-tested FIDO2 implementation in
existence. But the cost is the dealbreaker against heddle's no-heavy-FFI lean: on
Linux/macOS it *builds libfido2 from C source* (cmake + a C compiler + OpenSSL /
libcbor / zlib via pkg-config) on every build that links it. That's a per-contributor
toolchain tax and a cross-distro fragility surface heddle has deliberately avoided
elsewhere. Reserve as a fallback only if `ctap-hid-fido2` proves to have a
correctness gap.

**`ctap-hid-fido2`.** Best fit for a CLI. Pure-Rust CTAP2 logic, exposes
`make_credential` and `get_assertion`, ships a working `ctapcli` reference, actively
maintained (v3.5.11, May 2026), crypto via `ring`/`aws-lc-rs`. Its only C dependency
is `hidapi` (a tiny, ubiquitous, native-backend HID lib — *not* the Yubico C stack),
which is an acceptable transport dep. Limitations: **USB-HID only** (no NFC/BLE — fine
for a desktop CLI; revisit if mobile/remote ever matters) and the usual Linux
udev-rule provisioning for unprivileged HID access.

**`webauthn-authenticator-rs`.** It *is* client-side (not RP-only — that's
`webauthn-rs` proper), and it *does* expose the raw CTAP2 request structs, so it
isn't disqualified on shape. It's disqualified on **maturity**: its own docs call it
pre-1.0 / "alpha", explicitly lacking thorough security review and FIDO
certification, with an unstable API. For a *security substrate* that signs durable
authorship identity, an un-reviewed, self-flagged-unstable crypto path is the wrong
trade — exactly the case where the [[cli-ergonomics-over-feature-count]] /
security-sensitivity bar says no. Its broad transport coverage (NFC/BLE/caBLE) is the
one reason to keep an eye on it for a future phase.

---

## 4. Recommended integration shape (for #38, when greenlit)

Build a `HardwareSigner` on `ctap-hid-fido2`, gated behind a **non-default
`hardware-key` Cargo feature** so the HID/C dependency never enters a default build
(consistent with heddle's feature discipline). It is a **new signature algorithm**,
not a fourth `Signer` backend.

```text
crates/crypto/
  src/
    fido2.rs              # feature = "hardware-key" only
      struct HardwareSigner { credential_id, public_key (COSE EdDSA), token handle }
      impl HardwareSigner {
        fn provision(...)  // CTAP2 make_credential: EdDSA resident credential,
                           //   refuse-with-advice if token can't do EdDSA
        fn assert(challenge: &[u8;32]) -> Assertion
                           // CTAP2 get_assertion; returns { signature, authenticator_data }
      }
```

Two non-negotiable design points, both flowing from §2:

1. **New algorithm discriminator, not the `Signer` trait.** The on-disk signature
   record gains a `fido2-assertion` algorithm. Its payload is
   `{ signature, authenticator_data }` (the bare `Vec<u8>` won't fit). Verify
   reconstructs `authenticator_data || SHA-256(challenge==content_hash)` and runs
   COSE-EdDSA verify. Add a `fido2-assertion` arm to `verify_payload_signature`
   (`crates/crypto/src/lib.rs:102-114`) and register the new on-disk format in the
   heddle#451 versioning inventory (it's a new signature shape → a versioned
   surface). Do **not** try to make `HardwareSigner` implement the existing
   `Signer` trait — that's the trap §1 warns about.

2. **No hangs on unplug; presence/PIN surface to the CLI.** Wrap `get_assertion` in
   a bounded operation with a clear "tap your key" prompt and a timeout; map
   token-absent / user-declined / PIN-required into typed CLI errors with
   actionable messages (a #38 AC). Never block the CLI indefinitely on a missing or
   unplugged token.

**Provisioning UX:** `heddle identity create --hardware` (or similar) runs
`make_credential` once, stores the credential-id + COSE public key in the heddle
identity record (the *public* half only), and from then on signing taps the key.
Prefer a **discoverable/resident** credential so the identity survives a fresh
machine with just the token.

---

## 5. Acceptance criteria the #38 follow-up carries

When #38 is greenlit, it inherits these ACs (decision-doc DoD → impl DoD):

- [ ] **Dependency is feature-gated.** `ctap-hid-fido2` enters only under a
      non-default `hardware-key` feature; `cargo build --workspace` (default
      features) does **not** pull HID/C deps. (CI's first build is default-features.)
- [ ] **Provision an EdDSA resident credential.** `HardwareSigner::provision` runs
      CTAP2 `make_credential` requesting COSE EdDSA (`-8`), stores credential-id +
      public key, and **refuses-with-advice** on a token lacking EdDSA (no silent
      P-256 fallback under an "ed25519" identity).
- [ ] **Sign + verify round-trips through heddle's path.** A hardware assertion over
      a state content-hash is persisted as the new `fido2-assertion` algorithm and
      **verifies** via `verify_payload_signature` (reconstructed
      `authenticator_data || SHA-256(challenge)`, COSE-EdDSA verify). A
      software-signed state and a hardware-signed state are both valid under the same
      verify entry point.
- [ ] **New on-disk signature format registered in heddle#451's inventory** with a
      version discriminator and a refuse-with-advice old↦new story.
- [ ] **No-hang-on-unplug.** Token-absent / unplugged-mid-op / user-declined /
      PIN-required each produce a typed, actionable CLI error within a bounded
      timeout — never an indefinite block.
- [ ] **Platform coverage:** Linux + macOS first-class (Linux ships/install-docs a
      udev rule for unprivileged HID); Windows best-effort. (Matches the #38 AC.)
- [ ] **Scope guard:** hardware key backs the **durable authorship/state-signing
      identity only**; the biscuit `cnf` / device-PoP key stays software (no
      button-tap on every delegated agent call).
- [ ] **Cross-engine review** of the crypto path (security substrate → mandatory
      independent review, per the security-sensitivity dispatch rule).

---

## 6. Crate facts (verified June 2026)

| Crate | Latest ver | Last release | Repo | Notes |
|---|---|---|---|---|
| `ctap-hid-fido2` | **3.5.11** | 2026-05-22 | github.com/gebogebogebo/ctap-hid-fido2 | pure-Rust CTAP 2.0/2.1 over `hidapi`; crypto `ring`/`aws-lc-rs`; macOS/Windows(admin)/Linux+RPi; `make_credential`+`get_assertion`; ships `ctapcli` |
| `fido2-rs` | **0.5.0** | 2026-04-16 | github.com/tyan-boot/fido-rs | Rust-2024; wraps `libfido2-sys` 0.5.0 → builds Yubico **libfido2 C** from source (non-MSVC) / prebuilt DLL (MSVC) |
| `libfido2-sys` | **0.5.x** | (tracks fido2-rs) | github.com/tyan-boot/fido-rs | raw FFI to Yubico libfido2 (upstream C lib current release 1.17.0) |
| `webauthn-authenticator-rs` | **0.5.5** | (kanidm) | github.com/kanidm/webauthn-rs | client-side; CTAP 2.0/2.1/2.1-PRE; USB/NFC/BLE/caBLE/Win10; **self-described pre-1.0 "alpha", no security review, no FIDO cert, unstable API** |
| `hidapi` (transport dep of ctap-hid-fido2) | (current) | — | github.com/ruabmbua/hidapi-rs → libusb/hidapi | multi-platform HID (Win/Linux/macOS/FreeBSD); Linux needs a udev rule for unprivileged access; macOS shared-device opt-in since 0.12 |

Assumption flagged: exact download counts and license SPDX per crate were not
re-verified at write time (crates.io renders client-side and resisted scrape);
repos/licenses are the standard permissive (MIT/Apache-2.0) for all four — confirm at
impl time before adding any to `deny.toml` / the cargo-deny allowlist (heddle's
git-dep / source gates, per the cargo-deny notes).

---

## 7. References

- heddle `Signer` trait + verify path: `crates/crypto/src/lib.rs:31-36`,
  `:41-60` (`load_signer`), `:102-114` (`verify_payload_signature`),
  `:70-81` (key-perm floor).
- Software identity / biscuit attenuation: `docs/SELF_SOVEREIGN_AUTH.md`,
  `crates/client/src/device_flow.rs`.
- On-disk format-stability policy this work must register against:
  `docs/spikes/heddle-451-schema-versioning-policy.md`.
- Issue #38 (impl, blocked by this spike); weft#183 (hardware-key recovery).
