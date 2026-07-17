// SPDX-License-Identifier: Apache-2.0
//! Bearer-token proof-of-possession signing.

use crate::{Ed25519Signer, Signer, SignerError, verify_payload_signature};

pub const POP_V2_DOMAIN: &str = "heddle-bearer-pop-v2";
pub const HDR_PROOF: &str = "x-heddle-proof";
pub const HDR_PROOF_TS: &str = "x-heddle-proof-ts";
pub const HDR_PROOF_NONCE: &str = "x-heddle-proof-nonce";

/// Canonical bytes the v2 PoP signature covers. Each field line is
/// `key=<byte-len>:<value>\n`; `<byte-len>` is computed from
/// `value.as_bytes().len()`.
pub fn pop_canonical_payload(
    token: &str,
    proof_ts: &str,
    method: &str,
    path: &str,
    nonce: &str,
) -> Vec<u8> {
    let mut out = String::with_capacity(
        POP_V2_DOMAIN.len()
            + 1
            + field_len("token", token)
            + field_len("proof_ts", proof_ts)
            + field_len("method", method)
            + field_len("path", path)
            + field_len("nonce", nonce),
    );
    out.push_str(POP_V2_DOMAIN);
    out.push('\n');
    push_field(&mut out, "token", token);
    push_field(&mut out, "proof_ts", proof_ts);
    push_field(&mut out, "method", method);
    push_field(&mut out, "path", path);
    push_field(&mut out, "nonce", nonce);
    out.into_bytes()
}

/// Sign a v2 bearer-token proof-of-possession payload.
pub fn sign_pop(
    signer: &Ed25519Signer,
    token: &str,
    proof_ts: &str,
    method: &str,
    path: &str,
    nonce: &str,
) -> Result<Vec<u8>, SignerError> {
    let payload = pop_canonical_payload(token, proof_ts, method, path, nonce);
    signer.sign(&payload)
}

/// Verify a v2 bearer-token proof-of-possession signature.
pub fn verify_pop(
    public_key: &[u8],
    token: &str,
    proof_ts: &str,
    method: &str,
    path: &str,
    nonce: &str,
    sig: &[u8],
) -> Result<(), SignerError> {
    let payload = pop_canonical_payload(token, proof_ts, method, path, nonce);
    verify_payload_signature(&payload, "ed25519", public_key, sig)
}

fn push_field(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push('=');
    out.push_str(&utf8_byte_len(value).to_string());
    out.push(':');
    out.push_str(value);
    out.push('\n');
}

fn field_len(key: &str, value: &str) -> usize {
    key.len() + 1 + utf8_byte_len(value).to_string().len() + 1 + value.len() + 1
}

#[allow(clippy::needless_as_bytes)]
fn utf8_byte_len(value: &str) -> usize {
    value.as_bytes().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINNED_TOKEN: &str = "tok-é";
    const PINNED_PROOF_TS: &str = "1700000000";
    const PINNED_METHOD: &str = "POST";
    const PINNED_PATH: &str = "/heddle.api.v1alpha1.IdentityService/WhoAmI";
    const PINNED_NONCE: &str = "nøñce";
    const PINNED_CANONICAL: &str = concat!(
        "heddle-bearer-pop-v2\n",
        "token=6:tok-é\n",
        "proof_ts=10:1700000000\n",
        "method=4:POST\n",
        "path=43:/heddle.api.v1alpha1.IdentityService/WhoAmI\n",
        "nonce=7:nøñce\n",
    );

    #[test]
    fn pop_canonical_payload_matches_pinned_vector() {
        let got = pop_canonical_payload(
            PINNED_TOKEN,
            PINNED_PROOF_TS,
            PINNED_METHOD,
            PINNED_PATH,
            PINNED_NONCE,
        );
        assert_eq!(got, PINNED_CANONICAL.as_bytes());
    }

    #[test]
    fn sign_pop_verifies() {
        let signer = Ed25519Signer::generate().expect("generate signer");
        let sig = sign_pop(
            &signer,
            PINNED_TOKEN,
            PINNED_PROOF_TS,
            PINNED_METHOD,
            PINNED_PATH,
            PINNED_NONCE,
        )
        .expect("sign pop");

        verify_pop(
            signer.public_key(),
            PINNED_TOKEN,
            PINNED_PROOF_TS,
            PINNED_METHOD,
            PINNED_PATH,
            PINNED_NONCE,
            &sig,
        )
        .expect("verify pop");
    }
}
