//! Caller-supplied request credentials, independent of repository storage.
use std::time::{SystemTime, UNIX_EPOCH};

use api::{
    heddle::api::v1alpha1::{CallContext, RequestProof},
    v2::MethodDescriptor,
};
use crypto::{Ed25519Signer, Signer};

use crate::transport::{Authorize, Error};

/// Caller-selected authority. No variant discovers credentials or mints a key.
/// Bearer-only service and anonymous tiers stay distinct from signed callers.
#[derive(Clone)]
pub enum Credentials {
    Public,
    Bearer {
        /// Raw serialized Biscuit, directly from IssuedCredential.biscuit.
        biscuit: Vec<u8>,
        grant_envelope: Vec<u8>,
    },
    Signed {
        signer: std::sync::Arc<Ed25519Signer>,
        /// Empty during a public, key-proved registration ceremony.
        biscuit: Vec<u8>,
        grant_envelope: Vec<u8>,
    },
}

impl Authorize for Credentials {
    async fn context(
        &self,
        method: &'static MethodDescriptor,
        body: &[u8],
    ) -> Result<CallContext, Error> {
        let operation = method.client_operation_id(body)?.unwrap_or_default();
        if method.client_operation_id_required && operation.is_empty() {
            return Err(Error::Protocol("request requires an operation ID"));
        }
        let (biscuit, grant_envelope, signer) = match self {
            Self::Public => (&[][..], &[][..], None),
            Self::Bearer {
                biscuit,
                grant_envelope,
            } => {
                if biscuit.is_empty() {
                    return Err(Error::Protocol("bearer credential cannot be empty"));
                }
                (biscuit.as_slice(), grant_envelope.as_slice(), None)
            }
            Self::Signed {
                signer,
                biscuit,
                grant_envelope,
            } => (biscuit.as_slice(), grant_envelope.as_slice(), Some(signer)),
        };
        let mut context = CallContext {
            client_operation_id: operation.into(),
            bearer_capability: biscuit.to_vec(),
            bearer_grant_envelope: grant_envelope.to_vec(),
            ..Default::default()
        };
        let Some(signer) = signer else {
            return Ok(context);
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::Io(e.to_string()))?
            .as_millis();
        let timestamp = i64::try_from(timestamp).map_err(|_| Error::Protocol("invalid clock"))?;
        let identity = format!("principal:device-key:{}", hex::encode(signer.public_key()));
        // UUID v4 is an OS-random nonce; there is no authority generation here.
        let nonce = uuid::Uuid::new_v4().as_bytes().to_vec();
        let signature = signer
            .sign(&api::signing::unary_bytes(
                &identity,
                method.path,
                timestamp,
                &nonce,
                body,
            ))
            .map_err(|e| Error::Io(e.to_string()))?;
        context.request_proof = Some(RequestProof {
            algorithm: "ed25519".into(),
            signing_identity: identity,
            timestamp_millis: timestamp,
            nonce,
            signature,
        });
        Ok(context)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    use api::v2::client::Rpc;
    use prost::Message;

    use super::*;

    #[test]
    fn request_context_verifies_without_a_transport_rewriting_its_identity() {
        let signer = Ed25519Signer::from_seed(&[17; 32]).expect("fixture signer");
        let key: [u8; 32] = signer.public_key().try_into().expect("key");
        let credentials = Credentials::Signed {
            signer: std::sync::Arc::new(signer),
            biscuit: vec![0, 255, 7, 128],
            grant_envelope: b"grant fixture".to_vec(),
        };
        let body = crate::contract::RenameThreadRequest {
            client_operation_id: "same-durable-operation".into(),
            name: "renamed".into(),
            ..Default::default()
        }
        .encode_to_vec();
        let method = crate::rpc::ThreadServiceRenameThread::METHOD;
        let context = ready(credentials.context(method, &body)).expect("context");
        assert_eq!(
            context.bearer_capability,
            [0, 255, 7, 128],
            "Biscuit stays raw binary"
        );
        assert_eq!(context.bearer_grant_envelope, b"grant fixture");
        let time = context
            .request_proof
            .as_ref()
            .expect("proof")
            .timestamp_millis;
        crate::request_proof::verify(&context, method, &body, &key, time)
            .expect("context independently binds the request operation identity");
        let again = ready(credentials.context(method, &body)).expect("fresh attempt");
        assert_eq!(again.client_operation_id, context.client_operation_id);
        assert_ne!(
            again.request_proof.as_ref().expect("retry proof").nonce,
            context
                .request_proof
                .as_ref()
                .expect("original proof")
                .nonce
        );
        assert!(
            crate::request_proof::verify(
                &context,
                crate::rpc::ThreadServiceChangeLifecycle::METHOD,
                &body,
                &key,
                time
            )
            .is_err()
        );
        let changed = crate::contract::RenameThreadRequest {
            client_operation_id: "same-durable-operation".into(),
            name: "different intent".into(),
            ..Default::default()
        }
        .encode_to_vec();
        assert!(crate::request_proof::verify(&context, method, &changed, &key, time).is_err());
        assert!(crate::request_proof::verify(&context, method, &body, &[99; 32], time).is_err());
    }

    #[test]
    fn public_and_bearer_tiers_preserve_operation_identity_without_inventing_proof() {
        let method = crate::rpc::IdentityServiceCompleteEmailVerification::METHOD;
        let body = crate::contract::CompleteEmailVerificationRequest {
            client_operation_id: "mailbox-proof".into(),
            ..Default::default()
        }
        .encode_to_vec();
        for credentials in [
            Credentials::Public,
            Credentials::Bearer {
                biscuit: vec![0, 128, 255],
                grant_envelope: vec![17],
            },
        ] {
            let context = ready(credentials.context(method, &body)).expect("context");
            assert_eq!(context.client_operation_id, "mailbox-proof");
            assert!(context.request_proof.is_none());
            assert!(context.bearer_proof.is_none());
            if matches!(credentials, Credentials::Bearer { .. }) {
                assert_eq!(context.bearer_capability, [0, 128, 255]);
                assert_eq!(context.bearer_grant_envelope, [17]);
            } else {
                assert!(context.bearer_capability.is_empty());
            }
        }
        let empty = Credentials::Bearer {
            biscuit: vec![],
            grant_envelope: vec![],
        };
        assert!(matches!(
            ready(empty.context(method, &body)),
            Err(Error::Protocol("bearer credential cannot be empty"))
        ));
        assert!(matches!(
            ready(Credentials::Public.context(method, &[])),
            Err(Error::Protocol("request requires an operation ID"))
        ));
    }

    fn ready<T>(future: impl Future<Output = T>) -> T {
        match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("in-memory credential construction cannot wait on I/O"),
        }
    }
}
