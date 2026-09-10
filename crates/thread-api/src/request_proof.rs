//! Portable v2 request PoP. Hosts supply the effective key from an already
//! verified Biscuit and separately enforce account attachment, scope, current
//! revocation and durable nonce consumption. This check grants no authority.
use api::{heddle::api::v1alpha1::CallContext, v2::MethodDescriptor};
use crypto::Ed25519Signer;

use crate::transport::Error;

pub const PROOF_WINDOW_MILLIS: u64 = 60_000;

pub struct VerifiedProof<'a> {
    identity: &'a str,
    nonce: &'a [u8],
}
impl VerifiedProof<'_> {
    pub fn identity(&self) -> &str {
        self.identity
    }
    pub fn nonce(&self) -> &[u8] {
        self.nonce
    }
}

pub fn verify<'a>(
    context: &'a CallContext,
    method: &'static MethodDescriptor,
    body: &[u8],
    effective_key: &[u8; 32],
    now_millis: i64,
) -> Result<VerifiedProof<'a>, Error> {
    let operation_id = method.client_operation_id(body)?.unwrap_or_default();
    if operation_id != context.client_operation_id
        || (method.client_operation_id_required && operation_id.is_empty())
    {
        return Err(Error::Protocol(
            "request and context operation IDs differ or are missing",
        ));
    }
    let proof = context
        .request_proof
        .as_ref()
        .ok_or(Error::Protocol("request PoP required"))?;
    if proof.algorithm != "ed25519"
        || proof.nonce.len() != 16
        || now_millis.abs_diff(proof.timestamp_millis) > PROOF_WINDOW_MILLIS
    {
        return Err(Error::Protocol("invalid or expired request PoP"));
    }
    let identity = format!("principal:device-key:{}", hex::encode(effective_key));
    if proof.signing_identity != identity {
        return Err(Error::Protocol(
            "request signing identity differs from Biscuit",
        ));
    }
    Ed25519Signer::verify_with_public_key(
        &api::signing::unary_bytes(
            &identity,
            method.path,
            proof.timestamp_millis,
            &proof.nonce,
            body,
        ),
        effective_key,
        &proof.signature,
    )
    .map_err(|_| Error::Protocol("invalid request signature"))?;
    Ok(VerifiedProof {
        identity: &proof.signing_identity,
        nonce: &proof.nonce,
    })
}

#[cfg(test)]
mod tests {
    use api::{heddle::api::v1alpha1::RequestProof, v2::client::Rpc};
    use crypto::Signer;
    use prost::Message;

    use super::*;

    #[test]
    fn proof_binds_exact_method_bytes_effective_key_time_and_operation_identity() {
        let method = crate::rpc::ThreadServiceRenameThread::METHOD;
        let signer = Ed25519Signer::from_seed(&[17; 32]).expect("signer");
        let key: [u8; 32] = signer.public_key().try_into().expect("public key");
        let body = crate::contract::RenameThreadRequest {
            client_operation_id: "rename-1".into(),
            ..Default::default()
        }
        .encode_to_vec();
        let identity = format!("principal:device-key:{}", hex::encode(key));
        let nonce = vec![2; 16];
        let timestamp = 1_000_000;
        let signature = signer
            .sign(&api::signing::unary_bytes(
                &identity,
                method.path,
                timestamp,
                &nonce,
                &body,
            ))
            .expect("signing");
        let context = CallContext {
            client_operation_id: "rename-1".into(),
            request_proof: Some(RequestProof {
                algorithm: "ed25519".into(),
                signing_identity: identity.clone(),
                nonce: nonce.clone(),
                timestamp_millis: timestamp,
                signature,
            }),
            ..Default::default()
        };
        let verified = verify(&context, method, &body, &key, timestamp).expect("valid PoP");
        assert_eq!(verified.identity(), identity);
        assert_eq!(verified.nonce(), nonce);
        let mut changed = body.clone();
        changed.extend([0xa0, 0x06, 1]); // valid unknown protobuf field
        assert!(verify(&context, method, &changed, &key, timestamp).is_err());
        assert!(
            verify(
                &context,
                crate::rpc::ThreadServiceChangeLifecycle::METHOD,
                &body,
                &key,
                timestamp
            )
            .is_err()
        );
        assert!(verify(&context, method, &body, &[8; 32], timestamp).is_err());
        assert!(verify(&context, method, &body, &key, timestamp + 60_001).is_err());
        assert!(verify(&context, method, &body, &key, timestamp - 60_001).is_err());
        let mut wrong_context = context.clone();
        wrong_context.client_operation_id = "different".into();
        assert!(verify(&wrong_context, method, &body, &key, timestamp).is_err());
        wrong_context = context;
        wrong_context
            .request_proof
            .as_mut()
            .expect("proof")
            .signing_identity = "principal:mutable-handle".into();
        assert!(verify(&wrong_context, method, &body, &key, timestamp).is_err());
    }
}
