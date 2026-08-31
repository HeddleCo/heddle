# heddle-claim/1 — published JSON contract

Consumable contract for the **two-phase browser claim protocol**. Browser/TS
clients (tapestry, iroh-web) should generate their types and test fixtures from
the files here instead of hand-mirroring the private Rust enums in
`crates/hosted-client/src/hosted_runtime/claim_authorization.rs`.

## Files

| File | What it is |
|---|---|
| `heddle-claim-v1.schema.json` | JSON Schema (draft 2020-12) for every request/reply shape. Feed it to `json-schema-to-typescript`, `ajv`, `quicktype`, etc. |
| `heddle-claim-v1.golden.json` | Golden vectors — one concrete example per request and reply variant, plus the protocol/method routing. Use these as cross-language test fixtures. |

## Divergence guard

`claim_authorization.rs` contains a `#[cfg(test)] mod contract` test that
serializes every `ClaimReply` variant and parses every `ClaimRequest` variant
through the **real serde enums**, asserting they match `heddle-claim-v1.golden.json`
byte-for-value. If the Rust enums change a field name, a `#[serde(rename)]`, the
`kind` tag, or the camelCase casing, that test goes red. Regenerate/adjust the
golden file only alongside the Rust change. The schema is maintained by hand
alongside the golden vectors and the same test asserts every golden vector is
one of the schema's declared variants.

## Transport

- **ALPN:** `heddle-claim/1` (iroh)
- **Service:** `heddle.claim.v1.ClaimService`
- **Encoding:** `serde_json` — **not** protobuf. Request bodies are
  `serde_json::from_slice`; reply bodies are `#[derive(Serialize)]`.
- **Enum representation:** internally tagged on the `kind` discriminant; variant
  names and all fields are `camelCase`; requests reject unknown fields.
- **Claim secret:** the one-time claim secret rides in
  `CallContext.bearer_capability` on the transport frame — **never** in a JSON body.

## Two-phase flow

| Method | Request `kind` | Reply `kind` |
|---|---|---|
| `/heddle.claim.v1.ClaimService/Resolve` | `resolve` | `resolved` (or `refused` when already claimed) |
| `/heddle.claim.v1.ClaimService/Consent` | `preConsent` `{ handle, nonce }` | `preConsented` `{ consent }` |
| `/heddle.claim.v1.ClaimService/Consent` | `promoteConsent` `{ handle, credentialId }` | `promoteConsented` `{ consent }` |
| `/heddle.claim.v1.ClaimService/ClaimOwnerRoot` | `resolveOwnerRoot` `{ handle }` | `ownerRootResolved` `{ signedOwnerRoot, webauthnChallenge }` |
| `/heddle.claim.v1.ClaimService/ClaimOwnerRoot` | `claimOwnerRoot` `{ ... }` | `ownerRootCoSigned` `{ signedTransition }` |

`preConsent` binds the ceremony (`state.prepare(handle, nonce)`); `promoteConsent`
must match its own pre-consent (`state.claim(handle)`). Both consent replies carry
a signed `AgentConsent` tuple; a browser must present the matching pre/promote pair
to weft to attach the passkey root.

> Values in the golden vectors are representative examples. Only the **shape**
> (keys, casing, `kind` tag) is contractual — not the specific string values.
