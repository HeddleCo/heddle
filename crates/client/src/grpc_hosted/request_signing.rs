//! Client-side request signing for the gRPC boundary (heddle#/weft#346, #338 phase F).
//!
//! This is the CLIENT half of the request-signing epic. The server contract
//! (canonical bytes, headers, verification) lives in `weft-server`'s
//! `request_signature` module (weft#341/#343/#344); this module MUST produce
//! bytes and headers that match it byte-for-byte, or every signed call is
//! rejected.
//!
//! # What it does
//!
//! For every unary request the hosted client sends, we proactively attach a
//! Tier-1 proof-of-possession (PoP) signature over the outgoing request:
//!
//! 1. Deterministically encode the protobuf request.
//! 2. Build the contract-owned `heddle-req-sig-v1` canonical bytes, binding
//!    the stable signing identity, actual route, timestamp, nonce, and request hash.
//! 3. Sign the canonical bytes with the device Ed25519 key (the SAME key the
//!    client already uses for the `x-heddle-proof` bearer proof-of-possession).
//! 4. Attach the `x-heddle-sig-*` PoP headers.
//!
//! Signing is proactive on *all* unary calls: the server ignores signatures on
//! unsigned-tier RPCs, so the client needs no server tier-map. A client with no
//! device key (anonymous / unauthenticated) simply skips signing — no panic.
//!
//! # Human tier
//!
//! When a signed-tier-`human` RPC is rejected with `UNAUTHENTICATED` and the
//! trailer `x-heddle-sig-required: human`, the caller can re-sign the SAME action
//! with a WebAuthn assertion (Tier-2) via an app-registered
//! [`HumanSignatureCallback`] and retry once. The WebAuthn challenge is
//! *client-derived* — `SHA256(canonical bytes)` — with no server round-trip
//! (ratified on weft#338, 2026-07-03). See [`super::session`] retry wiring.

use base64::Engine as _;
use crypto::{Ed25519Signer, Signer as _};
use grpc::signing;
use sha2::{Digest, Sha256};
use tonic::{
    Request,
    metadata::{Ascii, BinaryMetadataValue, MetadataValue},
};
use wire::ProtocolError;

/// Domain-separation prefix. MUST match `weft-server`'s `DOMAIN_PREFIX`.
/// PoP header names. MUST match `weft-server`'s `request_signature` middleware.
pub(super) const HDR_SIG_ALG: &str = signing::HEADER_ALGORITHM;
pub(super) const HDR_SIG_BIN: &str = signing::HEADER_SIGNATURE_BIN;
pub(super) const HDR_SIG_TS: &str = signing::HEADER_TIMESTAMP;
pub(super) const HDR_SIG_NONCE_BIN: &str = signing::HEADER_NONCE_BIN;
pub(super) const HDR_SIG_IDENTITY: &str = signing::HEADER_IDENTITY;

/// WebAuthn (human-tier) header names. MUST match `weft-server`.
pub(super) const HDR_SIG_WEBAUTHN_CLIENT_DATA_BIN: &str = signing::HEADER_WEBAUTHN_CLIENT_DATA_BIN;
pub(super) const HDR_SIG_WEBAUTHN_AUTH_DATA_BIN: &str = signing::HEADER_WEBAUTHN_AUTH_DATA_BIN;
pub(super) const HDR_SIG_WEBAUTHN_USER_HANDLE_BIN: &str = signing::HEADER_WEBAUTHN_USER_HANDLE_BIN;

/// Discovery trailer/header the server sets on a signature-required rejection.
pub(super) const HDR_SIG_REQUIRED: &str = signing::HEADER_REQUIRED;

/// Deep-link trailer (weft#338): on a human-tier rejection the server MAY set this to the
/// tapestry `/verify-action` URL where the user can complete the action. Present only when the
/// server has a web origin configured; the client surfaces it in the human-signature callback.
/// MUST match `weft-server`'s `HDR_SIG_ACTION_URL`.
pub(super) const HDR_SIG_ACTION_URL: &str = signing::HEADER_ACTION_URL;

/// Alg tag values.
const ALG_ED25519: &str = "ed25519";
const ALG_WEBAUTHN: &str = "webauthn";

/// CSPRNG nonce length (server requires `>= 16`).
const NONCE_LEN: usize = 16;

/// Build the contract-owned `heddle-req-sig-v1` canonical byte string.
pub fn canonical_bytes(
    signing_identity: &str,
    path: &str,
    ts_millis: i64,
    nonce: &[u8],
    body: &[u8],
) -> Vec<u8> {
    signing::unary_bytes(signing_identity, path, ts_millis, nonce, body)
}

/// Client-derived WebAuthn challenge for the human tier: base64url (no pad) of
/// `SHA256(canonical)`. MUST match `weft-server`'s `request_signature_challenge`.
pub fn human_challenge(canonical: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(canonical))
}

/// Generate a fresh CSPRNG nonce (`NONCE_LEN` bytes).
fn fresh_nonce() -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    rand::fill(&mut nonce);
    nonce
}

fn now_millis() -> Result<i64, ProtocolError> {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
    i64::try_from(dur.as_millis())
        .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))
}

fn ascii(value: impl AsRef<str>) -> Result<MetadataValue<Ascii>, ProtocolError> {
    MetadataValue::try_from(value.as_ref())
        .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))
}

/// The inputs and outputs of a single PoP signing operation. Exposed so the
/// human-tier retry can reuse the *same* action (path/ts/nonce/body-hash → same
/// canonical → same challenge) that the PoP attempt covered.
pub(super) struct SignedRequestContext {
    pub canonical: Vec<u8>,
    pub ts_millis: i64,
    pub nonce: [u8; NONCE_LEN],
}

/// Sign an outgoing unary request with the device Ed25519 key and attach the
/// Tier-1 PoP headers. Returns the signing context so a human-tier retry can
/// derive the identical WebAuthn challenge.
///
/// `message_bytes` must be the *prost-encoded* request message (see the call
/// site in `apply_signature`); it is framed here to match the server's hash.
pub(super) fn attach_pop<T>(
    request: &mut Request<T>,
    signer: &Ed25519Signer,
    path: &str,
    message_bytes: &[u8],
) -> Result<SignedRequestContext, ProtocolError> {
    let ts_millis = now_millis()?;
    let nonce = fresh_nonce();
    let signing_identity = hex::encode(signer.public_key());
    let canonical = canonical_bytes(&signing_identity, path, ts_millis, &nonce, message_bytes);

    let signature = signer
        .sign(&canonical)
        .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;

    let md = request.metadata_mut();
    md.insert(HDR_SIG_ALG, ascii(ALG_ED25519)?);
    md.insert(HDR_SIG_TS, ascii(ts_millis.to_string())?);
    md.insert_bin(HDR_SIG_BIN, BinaryMetadataValue::from_bytes(&signature));
    md.insert_bin(HDR_SIG_NONCE_BIN, BinaryMetadataValue::from_bytes(&nonce));
    md.insert(HDR_SIG_IDENTITY, ascii(&signing_identity)?);

    Ok(SignedRequestContext {
        canonical,
        ts_millis,
        nonce,
    })
}

/// A WebAuthn assertion produced by an app-registered [`HumanSignatureCallback`]
/// to satisfy a human-tier RPC. All fields are raw bytes (base64-on-the-wire is
/// handled by the transport).
#[derive(Clone, Debug)]
pub struct WebAuthnAssertion {
    /// The credential id (rides in `x-heddle-sig-key-bin` for the human tier).
    pub credential_id: Vec<u8>,
    /// The assertion signature (`x-heddle-sig-bin`).
    pub signature: Vec<u8>,
    /// `clientDataJSON` (`x-heddle-sig-webauthn-client-data-bin`).
    pub client_data_json: Vec<u8>,
    /// `authenticatorData` (`x-heddle-sig-webauthn-auth-data-bin`).
    pub authenticator_data: Vec<u8>,
    /// Optional user handle (`x-heddle-sig-webauthn-user-handle-bin`).
    pub user_handle: Option<Vec<u8>>,
}

/// The action a human is being asked to authorize. Handed to the
/// [`HumanSignatureCallback`] so the app can render a consent surface before
/// running the WebAuthn ceremony.
#[derive(Clone, Debug)]
pub struct HumanSignatureRequest {
    /// Full gRPC method path, e.g. `/heddle.api.v1alpha1.RegistryService/DeleteSpool`.
    pub method_path: String,
    /// Human-readable one-line description of the action (best-effort).
    pub action_summary: String,
    /// The client-derived WebAuthn challenge = base64url(SHA256(canonical)).
    /// The authenticator must sign over exactly this challenge.
    pub challenge: String,
    /// The raw canonical bytes the challenge is derived from (for callbacks that
    /// want to re-derive or display the covered action).
    pub canonical: Vec<u8>,
    /// Deep-link to the surface that CAN complete this action (weft#338), taken
    /// verbatim from the server's `x-heddle-sig-action-url` rejection trailer, e.g.
    /// `https://app.heddle.sh/verify-action?method=...&challenge=...`. `None` when
    /// the server did not send it (no web origin configured) — callbacks then fall
    /// back to generic guidance. It is a display hint only; the challenge the
    /// callback signs over is still the client-derived [`Self::challenge`].
    pub action_url: Option<String>,
}

/// App-registered callback that turns a [`HumanSignatureRequest`] into a
/// [`WebAuthnAssertion`]. The CLI wires a terminal-prompt + platform-authenticator
/// implementation; tapestry wires a browser WebAuthn ceremony. If no callback is
/// registered, human-tier RPCs fail with a typed
/// [`ProtocolError::HumanSignatureRequired`]-shaped error rather than looping.
pub type HumanSignatureCallback = std::sync::Arc<
    dyn Fn(HumanSignatureRequest) -> Result<WebAuthnAssertion, ProtocolError> + Send + Sync,
>;

/// Attach the Tier-2 (human) WebAuthn headers from a callback-produced
/// [`WebAuthnAssertion`]. Overwrites the Tier-1 PoP headers for the retry.
pub(super) fn attach_human<T>(
    request: &mut Request<T>,
    ctx: &SignedRequestContext,
    assertion: &WebAuthnAssertion,
) -> Result<(), ProtocolError> {
    let md = request.metadata_mut();
    md.insert(HDR_SIG_ALG, ascii(ALG_WEBAUTHN)?);
    md.insert(HDR_SIG_TS, ascii(ctx.ts_millis.to_string())?);
    md.insert_bin(
        HDR_SIG_NONCE_BIN,
        BinaryMetadataValue::from_bytes(&ctx.nonce),
    );
    md.insert(
        HDR_SIG_IDENTITY,
        ascii(hex::encode(&assertion.credential_id))?,
    );
    md.insert_bin(
        HDR_SIG_BIN,
        BinaryMetadataValue::from_bytes(&assertion.signature),
    );
    md.insert_bin(
        HDR_SIG_WEBAUTHN_CLIENT_DATA_BIN,
        BinaryMetadataValue::from_bytes(&assertion.client_data_json),
    );
    md.insert_bin(
        HDR_SIG_WEBAUTHN_AUTH_DATA_BIN,
        BinaryMetadataValue::from_bytes(&assertion.authenticator_data),
    );
    if let Some(user_handle) = &assertion.user_handle {
        md.insert_bin(
            HDR_SIG_WEBAUTHN_USER_HANDLE_BIN,
            BinaryMetadataValue::from_bytes(user_handle),
        );
    }
    Ok(())
}

/// Does this `Status` signal a human-tier signature requirement? True iff the
/// code is `UNAUTHENTICATED` and the `x-heddle-sig-required` trailer/header is
/// `human`.
pub(super) fn requires_human_signature(status: &tonic::Status) -> bool {
    if status.code() != tonic::Code::Unauthenticated {
        return false;
    }
    status
        .metadata()
        .get(HDR_SIG_REQUIRED)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("human"))
        .unwrap_or(false)
}

/// Extract the server's `x-heddle-sig-action-url` deep-link (weft#338) from a rejection
/// `Status`, if present. `None` when the trailer is absent (server has no web origin
/// configured) or non-ASCII. Read from the same rejection metadata as
/// [`requires_human_signature`].
pub(super) fn action_url_from_status(status: &tonic::Status) -> Option<String> {
    status
        .metadata()
        .get(HDR_SIG_ACTION_URL)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The pinned test vector from weft#341 (`request_signature.rs`
    // `canonical_bytes_pinned_test_vector`). If this ever drifts, the client
    // and server disagree on the signed bytes and every signed call is
    // rejected — this is a conformance gate, not a sanity check.
    const PINNED_IDENTITY: &str = "principal:alice";
    const PINNED_PATH: &str = "/heddle.api.v1alpha1.RegistryService/DeleteRepository";
    const PINNED_TS: i64 = 1_784_059_200_123;
    const PINNED_NONCE: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];
    const PINNED_REQUEST: &[u8] = b"\x0a\x03foo";
    const PINNED_CANONICAL_HEX: &str = "686564646c652d7265712d7369672d76310a6b696e643d353a756e6172790a6964656e746974793d31353a7072696e636970616c3a616c6963650a726f7574653d35333a2f686564646c652e6170692e7631616c706861312e5265676973747279536572766963652f44656c6574655265706f7369746f72790a74696d657374616d705f6d733d31333a313738343035393230303132330a6e6f6e63653d33323a30303031303230333034303530363037303830393061306230633064306530660a726571756573745f7368613235363d36343a32323362343037323164663030383839636334306637636137393534613532343763656434613964666464613634393766333230343663393631383562646163";

    #[test]
    fn canonical_bytes_matches_weft341_pinned_vector() {
        let got = canonical_bytes(
            PINNED_IDENTITY,
            PINNED_PATH,
            PINNED_TS,
            &PINNED_NONCE,
            PINNED_REQUEST,
        );
        assert_eq!(hex::encode(got), PINNED_CANONICAL_HEX);
    }

    #[test]
    fn canonical_bytes_change_any_field_changes_output() {
        let base = canonical_bytes(PINNED_IDENTITY, PINNED_PATH, PINNED_TS, &PINNED_NONCE, b"");
        assert_ne!(
            canonical_bytes(PINNED_IDENTITY, "/other", PINNED_TS, &PINNED_NONCE, b""),
            base
        );
        assert_ne!(
            canonical_bytes(
                PINNED_IDENTITY,
                PINNED_PATH,
                PINNED_TS + 1,
                &PINNED_NONCE,
                b""
            ),
            base
        );
        assert_ne!(
            canonical_bytes(PINNED_IDENTITY, PINNED_PATH, PINNED_TS, &[0xff; 16], b""),
            base
        );
        assert_ne!(
            canonical_bytes(PINNED_IDENTITY, PINNED_PATH, PINNED_TS, &PINNED_NONCE, b"x"),
            base
        );
    }

    #[test]
    fn attach_pop_produces_headers_verifiable_against_device_pubkey() {
        let signer = Ed25519Signer::generate().expect("gen device key");
        let pubkey = signer.public_key().to_vec();
        let path = "/heddle.api.v1alpha1.RegistryService/DeleteSpool";
        let message_bytes = b"\x0a\x03abc"; // arbitrary encoded body

        let mut request = Request::new(());
        let ctx = attach_pop(&mut request, &signer, path, message_bytes).expect("attach pop");

        let md = request.metadata();
        assert_eq!(
            md.get(HDR_SIG_ALG).and_then(|v| v.to_str().ok()),
            Some("ed25519")
        );
        // ts header echoes the signed ts.
        assert_eq!(
            md.get(HDR_SIG_TS).and_then(|v| v.to_str().ok()),
            Some(ctx.ts_millis.to_string().as_str())
        );

        // Recompute the canonical bytes exactly as the server would (framed
        // body) and verify the attached signature against the device pubkey.
        let identity = hex::encode(&pubkey);
        let canonical = canonical_bytes(&identity, path, ctx.ts_millis, &ctx.nonce, message_bytes);
        assert_eq!(canonical, ctx.canonical);

        let sig_b64 = md
            .get_bin(HDR_SIG_BIN)
            .expect("sig header present")
            .to_bytes()
            .expect("sig decodes");
        Ed25519Signer::verify_with_public_key(&canonical, &pubkey, &sig_b64)
            .expect("attached signature verifies against the device pubkey");

        assert_eq!(
            md.get(HDR_SIG_IDENTITY)
                .and_then(|value| value.to_str().ok()),
            Some(identity.as_str())
        );

        // nonce header is >= 16 bytes.
        let nonce_bytes = md
            .get_bin(HDR_SIG_NONCE_BIN)
            .expect("nonce header present")
            .to_bytes()
            .expect("nonce decodes");
        assert!(nonce_bytes.len() >= 16);
    }

    #[test]
    fn human_challenge_is_client_derived_sha256_of_canonical() {
        let canonical = canonical_bytes(
            PINNED_IDENTITY,
            PINNED_PATH,
            PINNED_TS,
            &PINNED_NONCE,
            b"body",
        );
        let challenge = human_challenge(&canonical);
        let expected =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(&canonical));
        assert_eq!(challenge, expected);
    }

    #[test]
    fn requires_human_signature_detects_trailer() {
        let mut md = tonic::metadata::MetadataMap::new();
        md.insert(HDR_SIG_REQUIRED, "human".parse().unwrap());
        let status =
            tonic::Status::with_metadata(tonic::Code::Unauthenticated, "needs human", md.clone());
        assert!(requires_human_signature(&status));

        // pop-tier requirement is not a human requirement.
        let mut md_pop = tonic::metadata::MetadataMap::new();
        md_pop.insert(HDR_SIG_REQUIRED, "pop".parse().unwrap());
        let status_pop =
            tonic::Status::with_metadata(tonic::Code::Unauthenticated, "needs pop", md_pop);
        assert!(!requires_human_signature(&status_pop));

        // wrong code is not a human requirement even with the trailer.
        let status_wrong_code =
            tonic::Status::with_metadata(tonic::Code::PermissionDenied, "denied", md);
        assert!(!requires_human_signature(&status_wrong_code));
    }

    #[test]
    fn attach_human_sets_webauthn_headers() {
        let signer = Ed25519Signer::generate().expect("gen");
        let mut request = Request::new(());
        let ctx = attach_pop(&mut request, &signer, "/x/Y", b"body").expect("pop");

        let assertion = WebAuthnAssertion {
            credential_id: vec![1, 2, 3],
            signature: vec![4, 5, 6],
            client_data_json: b"{}".to_vec(),
            authenticator_data: vec![7; 37],
            user_handle: Some(vec![9]),
        };
        attach_human(&mut request, &ctx, &assertion).expect("attach human");
        let md = request.metadata();
        assert_eq!(
            md.get(HDR_SIG_ALG).and_then(|v| v.to_str().ok()),
            Some("webauthn")
        );
        assert_eq!(
            md.get(HDR_SIG_IDENTITY)
                .and_then(|value| value.to_str().ok()),
            Some("010203")
        );
        assert!(md.get_bin(HDR_SIG_WEBAUTHN_CLIENT_DATA_BIN).is_some());
        assert!(md.get_bin(HDR_SIG_WEBAUTHN_AUTH_DATA_BIN).is_some());
        assert!(md.get_bin(HDR_SIG_WEBAUTHN_USER_HANDLE_BIN).is_some());
    }
}
