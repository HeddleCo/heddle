use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use api::{
    heddle::api::v1alpha1::{
        BearerProof, CallContext, HumanVerification, RepositoryRef, RequestProof,
        StreamOpeningProof, TraceContext, repository_ref,
    },
    signing,
};
use cli_shared::ClientConfig;
use crypto::{Ed25519Signer, Signer as _};
use prost_types::Timestamp;

use super::{HostedError, Result};

const NONCE_LEN: usize = 16;

/// Transport-neutral authentication, deadline, and trace inputs for hosted calls.
///
/// This is the sole place where client configuration becomes contract-owned
/// [`CallContext`] data. Domain call sites provide only a method, request body,
/// and optional idempotency key.
#[derive(Clone)]
pub struct CallContextFactory {
    bearer_capability: Vec<u8>,
    bearer_grant_envelope: Vec<u8>,
    signer: Option<Arc<Ed25519Signer>>,
    signing_identity: Option<String>,
    timeout: Duration,
    trace: Option<TraceContext>,
}

impl std::fmt::Debug for CallContextFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CallContextFactory")
            .field("has_bearer", &!self.bearer_capability.is_empty())
            .field(
                "has_grant_envelope",
                &!self.bearer_grant_envelope.is_empty(),
            )
            .field("has_signer", &self.signer.is_some())
            .field("signing_identity", &self.signing_identity)
            .field("timeout", &self.timeout)
            .field("trace", &self.trace)
            .finish()
    }
}

impl Default for CallContextFactory {
    fn default() -> Self {
        Self {
            bearer_capability: Vec::new(),
            bearer_grant_envelope: Vec::new(),
            signer: None,
            signing_identity: None,
            timeout: Duration::from_secs(30),
            trace: None,
        }
    }
}

impl CallContextFactory {
    pub fn bearer_capability(&self) -> &[u8] {
        &self.bearer_capability
    }

    pub fn set_bearer_capability(&mut self, capability: impl Into<Vec<u8>>) {
        self.bearer_capability = capability.into();
    }

    pub fn signing_identity(&self) -> Option<&str> {
        self.signing_identity.as_deref()
    }

    pub fn with_bearer_capability(mut self, capability: impl Into<Vec<u8>>) -> Self {
        self.bearer_capability = capability.into();
        self
    }

    pub fn with_signing_key_pem(
        mut self,
        pem: &str,
        signing_identity: impl Into<String>,
    ) -> Result<Self> {
        let signing_identity = signing_identity.into();
        if !signing_identity
            .strip_prefix("principal:")
            .is_some_and(|subject| !subject.trim().is_empty())
        {
            return Err(HostedError::SigningIdentityRequired);
        }
        self.signer = Some(Arc::new(Ed25519Signer::from_pem(pem)?));
        self.signing_identity = Some(signing_identity);
        Ok(self)
    }

    pub fn from_client_config(config: &ClientConfig) -> Result<Self> {
        let signer = config
            .auth_proof_key_pem
            .as_deref()
            .map(Ed25519Signer::from_pem)
            .transpose()?
            .map(Arc::new);
        if signer.is_some() && config.authenticated_principal.is_none() {
            return Err(HostedError::SigningIdentityRequired);
        }
        Ok(Self {
            bearer_capability: config
                .token
                .as_ref()
                .map_or_else(Vec::new, |token| token.id.as_bytes().to_vec()),
            bearer_grant_envelope: Vec::new(),
            signer,
            signing_identity: config.authenticated_principal.clone(),
            timeout: Duration::from_secs(config.timeout_secs.max(1)),
            trace: None,
        })
    }

    pub fn with_grant_envelope(mut self, envelope: impl Into<Vec<u8>>) -> Self {
        self.bearer_grant_envelope = envelope.into();
        self
    }

    pub fn with_trace(mut self, trace: TraceContext) -> Self {
        self.trace = Some(trace);
        self
    }

    pub fn unary(
        &self,
        method: &str,
        deterministic_request: &[u8],
        client_operation_id: impl Into<String>,
    ) -> Result<SignedCallContext> {
        let mut context = self.base(method, client_operation_id.into())?;
        let signed = match (&self.signer, &self.signing_identity) {
            (Some(signer), Some(identity)) => {
                let timestamp_millis = now_millis()?;
                let nonce = fresh_nonce();
                let canonical = signing::unary_bytes(
                    identity,
                    method,
                    timestamp_millis,
                    &nonce,
                    deterministic_request,
                );
                context.request_proof = Some(RequestProof {
                    algorithm: "ed25519".to_string(),
                    signing_identity: identity.clone(),
                    timestamp_millis,
                    nonce: nonce.to_vec(),
                    signature: signer.sign(&canonical)?,
                });
                Some(SignedAction {
                    canonical,
                    timestamp_millis,
                    nonce,
                    signing_identity: identity.clone(),
                })
            }
            _ => None,
        };
        Ok(SignedCallContext { context, signed })
    }

    pub fn streaming(
        &self,
        method: &str,
        client_operation_id: impl Into<String>,
    ) -> Result<CallContext> {
        self.base(method, client_operation_id.into())
    }

    pub fn stream_opening_proof(
        &self,
        method: &str,
        stream_id: impl Into<String>,
        repository: RepositoryRef,
        resume_cursor: impl Into<String>,
        capability_context: Vec<u8>,
    ) -> Result<StreamOpeningProof> {
        let signer = self
            .signer
            .as_ref()
            .ok_or(HostedError::SigningIdentityRequired)?;
        let identity = self
            .signing_identity
            .as_deref()
            .ok_or(HostedError::SigningIdentityRequired)?;
        let repository_text = match repository.reference.as_ref() {
            Some(repository_ref::Reference::CanonicalPath(path))
            | Some(repository_ref::Reference::HostedId(path))
                if !path.is_empty() =>
            {
                path.as_str()
            }
            _ => return Err(HostedError::InvalidRepositoryReference),
        };
        let stream_id = stream_id.into();
        let resume_cursor = resume_cursor.into();
        let canonical = signing::stream_open_bytes(
            identity,
            &stream_id,
            method,
            repository_text,
            &resume_cursor,
            &capability_context,
        );
        Ok(StreamOpeningProof {
            stream_id,
            route: method.to_string(),
            repository: Some(repository),
            resume_cursor,
            capability_context,
            nonce: Vec::new(),
            signature: signer.sign(&canonical)?,
        })
    }

    fn base(&self, method: &str, client_operation_id: String) -> Result<CallContext> {
        Ok(CallContext {
            deadline: Some(deadline(self.timeout)?),
            bearer_capability: self.bearer_capability.clone(),
            bearer_proof: self.bearer_proof(method)?,
            request_proof: None,
            human_verification: None,
            client_operation_id,
            trace: self.trace.clone(),
            bearer_grant_envelope: self.bearer_grant_envelope.clone(),
        })
    }

    fn bearer_proof(&self, method: &str) -> Result<Option<BearerProof>> {
        let Some(signer) = &self.signer else {
            return Ok(None);
        };
        if self.bearer_capability.is_empty() {
            return Ok(None);
        }
        let timestamp_seconds = now_seconds()?;
        let nonce = fresh_nonce();
        let token = std::str::from_utf8(&self.bearer_capability)
            .map_err(|_| HostedError::InvalidBearerCapability)?;
        let signature = crypto::pop::sign_pop(
            signer,
            token,
            &timestamp_seconds.to_string(),
            "POST",
            method,
            &hex::encode(nonce),
        )?;
        Ok(Some(BearerProof {
            timestamp_seconds,
            nonce: nonce.to_vec(),
            signature,
        }))
    }
}

/// A context plus the stable action data required for one human-verification retry.
#[derive(Debug)]
pub struct SignedCallContext {
    pub context: CallContext,
    signed: Option<SignedAction>,
}

impl SignedCallContext {
    pub fn with_human_verification(
        mut self,
        signature: Vec<u8>,
        verification: HumanVerification,
    ) -> Result<CallContext> {
        let signed = self.signed.ok_or(HostedError::SigningIdentityRequired)?;
        self.context.request_proof = Some(RequestProof {
            algorithm: "webauthn".to_string(),
            signing_identity: signed.signing_identity,
            timestamp_millis: signed.timestamp_millis,
            nonce: signed.nonce.to_vec(),
            signature,
        });
        self.context.human_verification = Some(verification);
        Ok(self.context)
    }

    pub fn canonical(&self) -> Option<&[u8]> {
        self.signed
            .as_ref()
            .map(|signed| signed.canonical.as_slice())
    }
}

#[derive(Debug)]
struct SignedAction {
    canonical: Vec<u8>,
    timestamp_millis: i64,
    nonce: [u8; NONCE_LEN],
    signing_identity: String,
}

fn fresh_nonce() -> [u8; NONCE_LEN] {
    let mut nonce = [0; NONCE_LEN];
    rand::fill(&mut nonce);
    nonce
}

fn now() -> Result<Duration> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(HostedError::transport)
}

fn now_seconds() -> Result<i64> {
    i64::try_from(now()?.as_secs()).map_err(HostedError::transport)
}

fn now_millis() -> Result<i64> {
    i64::try_from(now()?.as_millis()).map_err(HostedError::transport)
}

fn deadline(timeout: Duration) -> Result<Timestamp> {
    let deadline = now()?
        .checked_add(timeout)
        .ok_or_else(|| HostedError::InvalidDescriptor("call deadline overflow".to_string()))?;
    Ok(Timestamp {
        seconds: i64::try_from(deadline.as_secs()).map_err(HostedError::transport)?,
        nanos: i32::try_from(deadline.subsec_nanos()).map_err(HostedError::transport)?,
    })
}

#[cfg(test)]
mod tests {
    use api::UNARY_SIGNING_V1_FIXTURE_JSON;
    use crypto::Signer as _;
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct Vector {
        identity: String,
        route: String,
        timestamp_millis: i64,
        nonce_hex: String,
        request_hex: String,
        canonical_hex: String,
    }

    #[test]
    fn shared_canonical_fixture_is_the_client_signing_contract() {
        let vector: Vector = serde_json::from_str(UNARY_SIGNING_V1_FIXTURE_JSON).unwrap();
        let canonical = signing::unary_bytes(
            &vector.identity,
            &vector.route,
            vector.timestamp_millis,
            &hex::decode(vector.nonce_hex).unwrap(),
            &hex::decode(vector.request_hex).unwrap(),
        );
        assert_eq!(hex::encode(canonical), vector.canonical_hex);
    }

    #[test]
    fn configured_factory_places_bearer_and_verifiable_request_proof_in_context() {
        let signer = Ed25519Signer::generate().unwrap();
        let config = ClientConfig::default()
            .with_token(wire::AuthToken::new("token", "alice"))
            .with_auth_proof_key_pem(signer.to_pem().unwrap())
            .with_authenticated_principal("principal:alice");
        let signed = CallContextFactory::from_client_config(&config)
            .unwrap()
            .unary("/heddle.api.v1alpha1.IdentityService/WhoAmI", &[], "")
            .unwrap();
        assert_eq!(signed.context.bearer_capability, b"token");
        let proof = signed.context.request_proof.unwrap();
        let canonical = signing::unary_bytes(
            &proof.signing_identity,
            "/heddle.api.v1alpha1.IdentityService/WhoAmI",
            proof.timestamp_millis,
            &proof.nonce,
            &[],
        );
        Ed25519Signer::verify_with_public_key(&canonical, signer.public_key(), &proof.signature)
            .unwrap();
        let bearer = signed.context.bearer_proof.unwrap();
        crypto::pop::verify_pop(
            signer.public_key(),
            "token",
            &bearer.timestamp_seconds.to_string(),
            "POST",
            "/heddle.api.v1alpha1.IdentityService/WhoAmI",
            &hex::encode(&bearer.nonce),
            &bearer.signature,
        )
        .unwrap();
    }

    #[test]
    fn stream_opening_proof_is_bound_to_route_repository_and_identity() {
        let signer = Ed25519Signer::generate().unwrap();
        let config = ClientConfig::default()
            .with_token(wire::AuthToken::new("token", "alice"))
            .with_auth_proof_key_pem(signer.to_pem().unwrap())
            .with_authenticated_principal("principal:alice");
        let repository = RepositoryRef {
            reference: Some(repository_ref::Reference::CanonicalPath(
                "acme/widgets".to_string(),
            )),
        };
        let proof = CallContextFactory::from_client_config(&config)
            .unwrap()
            .stream_opening_proof(
                "/heddle.api.v1alpha1.RepoSyncService/Pull",
                "stream-1",
                repository,
                "cursor-1",
                b"capability".to_vec(),
            )
            .unwrap();
        let canonical = signing::stream_open_bytes(
            "principal:alice",
            "stream-1",
            "/heddle.api.v1alpha1.RepoSyncService/Pull",
            "acme/widgets",
            "cursor-1",
            b"capability",
        );
        Ed25519Signer::verify_with_public_key(&canonical, signer.public_key(), &proof.signature)
            .unwrap();
    }
}
