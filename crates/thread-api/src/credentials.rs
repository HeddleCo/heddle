//! Caller-supplied request credentials, independent of repository storage.
use std::time::{SystemTime, UNIX_EPOCH};

use api::{
    heddle::api::v1alpha1::{CallContext, RequestProof},
    v2::MethodDescriptor,
};
use crypto::{Ed25519Signer, Signer};
#[cfg(any(feature = "native", feature = "root-attachment"))]
use prost::Message;

use crate::transport::{Authorize, Error};

/// Caller-selected authority. No variant discovers credentials or mints a key.
/// Bearer-only service and anonymous tiers stay distinct from signed callers.
/// Public-readable methods still carry proof for Signed callers. Only the host
/// may classify a verified bearer as anonymous; this client never strips an
/// account credential or retries it as public after authorization fails.
#[derive(Clone)]
pub enum Credentials {
    Public,
    #[cfg(any(feature = "native", feature = "root-attachment"))]
    OwnedDevice(OwnedDeviceCredentials),
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

/// Prepared owned-device credential; the public proof supplies the exact sealed
/// Biscuit and mint selector. The receiver independently pins account authority.
#[cfg(any(feature = "native", feature = "root-attachment"))]
#[derive(Clone)]
pub struct OwnedDeviceCredentials {
    signer: std::sync::Arc<Ed25519Signer>,
    biscuit: Vec<u8>,
    mint_root: Vec<u8>,
    authority: Vec<u8>,
}
impl Credentials {
    #[cfg(any(feature = "native", feature = "root-attachment"))]
    pub fn owned_device(
        signer: std::sync::Arc<Ed25519Signer>,
        authority: &[u8],
    ) -> Result<Self, Error> {
        use api::heddle::api::v2alpha1::ThreadControlAuthority;
        if authority.is_empty() || authority.len() > 64 * 1024 {
            return Err(Error::Protocol("owned-device authority proof bound"));
        }
        let proof = ThreadControlAuthority::decode(authority)
            .map_err(|error| Error::Io(error.to_string()))?;
        if proof.format != 1 || proof.encode_to_vec() != authority {
            return Err(Error::Protocol("canonical owned-device authority required"));
        }
        if proof.mint_root_public_key.len() != 32
            || proof.sealed_biscuit.is_empty()
            || proof.sealed_biscuit.len() > 64 * 1024
        {
            return Err(Error::Protocol("owned-device mint root or Biscuit bound"));
        }
        use base64::Engine as _;
        let biscuit = base64::engine::general_purpose::URL_SAFE
            .encode(&proof.sealed_biscuit)
            .into_bytes();
        if biscuit.len() > 64 * 1024 {
            return Err(Error::Protocol("encoded owned-device Biscuit bound"));
        }
        Ok(Self::OwnedDevice(OwnedDeviceCredentials {
            signer,
            biscuit,
            mint_root: proof.mint_root_public_key,
            authority: authority.to_vec(),
        }))
    }
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
            #[cfg(any(feature = "native", feature = "root-attachment"))]
            Self::OwnedDevice(value) => (value.biscuit.as_slice(), &[][..], Some(&value.signer)),
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
        #[cfg(any(feature = "native", feature = "root-attachment"))]
        if let Self::OwnedDevice(value) = self {
            context.bearer_authority_key_selector = value.mint_root.clone();
            context.bearer_authority_proof = value.authority.clone();
        }
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
    #[cfg(any(feature = "native", feature = "root-attachment"))]
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

    #[test]
    fn public_catalog_preserves_unsigned_and_account_proof_boundaries() {
        let method = crate::rpc::WorkspaceServiceObserveCatalog::METHOD;
        assert_eq!(
            method.signing_tier,
            api::heddle::api::v1alpha1::SigningTier::ProofIfAuthenticated
        );
        let mut request = crate::contract::ObserveCatalogRequest::default();
        crate::observation::ObservationRequest::options_mut(&mut request).mode =
            crate::contract::ObservationMode::Once as i32;
        let body = request.encode_to_vec();
        let public = ready(Credentials::Public.context(method, &body)).expect("public context");
        assert!(public.request_proof.is_none());
        assert!(public.bearer_capability.is_empty());
        let bearer = ready(
            Credentials::Bearer {
                biscuit: vec![7],
                grant_envelope: vec![],
            }
            .context(method, &body),
        )
        .expect("opaque bearer context");
        assert_eq!(bearer.bearer_capability, [7]);
        assert!(
            bearer.request_proof.is_none(),
            "host classifies bearer authority"
        );
        let signer = Ed25519Signer::from_seed(&[18; 32]).expect("fixture signer");
        let key = signer.public_key().try_into().expect("public key");
        let account = ready(
            Credentials::Signed {
                signer: std::sync::Arc::new(signer),
                biscuit: vec![8],
                grant_envelope: vec![],
            }
            .context(method, &body),
        )
        .expect("signed public read");
        assert_eq!(account.bearer_capability, [8]);
        let now = account
            .request_proof
            .as_ref()
            .expect("account still proves key")
            .timestamp_millis;
        crate::request_proof::verify(&account, method, &body, &key, now)
            .expect("valid account proof");
        let mut missing = account;
        missing.request_proof = None;
        assert!(
            matches!(
                crate::request_proof::verify(&missing, method, &body, &key, now),
                Err(Error::Protocol("request PoP required"))
            ),
            "strict verifier never downgrades account reads"
        );
        let event = crate::contract::CatalogEvent {
            frame: None,
            payload: Some(crate::contract::catalog_event::Payload::Removal(
                Default::default(),
            )),
        };
        assert!(crate::observation::ObservedEvent::is_removal(&event));
    }

    fn ready<T>(future: impl Future<Output = T>) -> T {
        match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("in-memory credential construction cannot wait on I/O"),
        }
    }
}
