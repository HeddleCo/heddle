//! Native hosted calls over Iroh.
//!
//! The module is the single seam for bootstrap verification, connection reuse,
//! ALPN negotiation, framing, cancellation, and transport-neutral failures.

mod bootstrap;
mod call;
mod collaboration;
mod connection;
mod content;
mod context;
mod credential;
mod error;
pub(crate) mod helpers;
mod human;
mod hydration;
mod methods;
pub mod monorepo;
pub(crate) mod operation_id;
mod session;
mod state_review;
mod sync;
mod user;

use std::sync::Arc;

use api::heddle::api::v1alpha1::CallContext;
pub use bootstrap::{DescriptorKeyring, VerifiedEndpointDescriptor, fetch_endpoint_descriptor};
pub use call::{BidirectionalRequestStream, BidirectionalStream, ServerStream, ServerStreamItem};
use cli_shared::ClientConfig;
pub use collaboration::{HostedDiscussion, HostedDiscussionTurn};
use connection::HostedConnection;
pub use context::{CallContextFactory, SignedCallContext};
pub use credential::{
    CredentialSource, ResolvedHostedCredential, resolve_active_bearer, resolve_hosted_credential,
};
use crypto::{Ed25519Signer, Signer as _};
pub use error::HostedError;
pub use human::{HumanSignatureCallback, HumanSignatureRequest, WebAuthnAssertion};
pub use hydration::{LazyHostedHydrator, register_hosted_factory};
use iroh::{Endpoint, EndpointAddr};
pub use methods::HostedRoutes;
use prost::Message;
pub use session::{HostedAuthMode, HostedSession};
pub use sync::{HostedRefEntry, PullObjectMix, PullProfile};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RenewableAuthorityCredential {
    token: String,
    credential_id: String,
}

impl RenewableAuthorityCredential {
    pub(super) fn from_stored(credential: &crate::credentials::ServerCredential) -> Option<Self> {
        let credential_id = credential.credential_id.clone()?;
        let signer = Ed25519Signer::from_pem(credential.private_key_pem.as_ref()?).ok()?;
        let biscuit =
            biscuit_auth::UnverifiedBiscuit::from_base64(credential.token.as_bytes()).ok()?;
        if biscuit.block_count() != 1 {
            return None;
        }
        let authority = biscuit_auth::builder::BlockBuilder::new()
            .code(&biscuit.print_block_source(0).ok()?)
            .ok()?;
        let credential_ids = authority
            .facts
            .iter()
            .filter_map(|fact| {
                match (
                    fact.predicate.name.as_str(),
                    fact.predicate.terms.as_slice(),
                ) {
                    ("credential_id", [biscuit_auth::builder::Term::Str(id)]) => Some(id),
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        let [token_credential_id] = credential_ids.as_slice() else {
            return None;
        };
        if token_credential_id.as_str() != credential_id.as_str()
            || !crate::device_flow::effective_pop_public_key_hex(&credential.token)
                .is_ok_and(|key| key.eq_ignore_ascii_case(&hex::encode(signer.public_key())))
        {
            return None;
        }
        Some(Self {
            token: credential.token.clone(),
            credential_id,
        })
    }

    fn matches_active_client(
        &self,
        credential: &crate::credentials::ServerCredential,
        active_bearer: &[u8],
    ) -> bool {
        credential.token == self.token
            && credential.credential_id.as_deref() == Some(self.credential_id.as_str())
            && active_bearer == self.token.as_bytes()
    }
}

pub type Result<T> = std::result::Result<T, HostedError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullMaterialization {
    Full,
    Lazy,
}

impl PullMaterialization {
    pub(crate) fn allows_partial_fetch(self) -> bool {
        matches!(self, Self::Lazy)
    }
}

/// One reusable native connection to a terminating Weft application endpoint.
#[derive(Clone)]
pub struct HostedClient {
    connection: Arc<HostedConnection>,
    context: CallContextFactory,
    transport: helpers::HostedTransportPolicy,
    on_human_signature: Option<HumanSignatureCallback>,
    server_key: Option<String>,
}

impl std::fmt::Debug for HostedClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostedClient")
            .field("connection", &self.connection)
            .field("context", &self.context)
            .field("transport", &self.transport)
            .field(
                "has_human_signature_callback",
                &self.on_human_signature.is_some(),
            )
            .finish()
    }
}

impl HostedClient {
    pub fn routes(&self) -> HostedRoutes<'_> {
        HostedRoutes::new(self)
    }

    pub async fn connect(descriptor: &VerifiedEndpointDescriptor) -> Result<Self> {
        Ok(Self {
            connection: HostedConnection::connect_verified(descriptor).await?,
            context: CallContextFactory::default(),
            transport: helpers::HostedTransportPolicy::from_client_config(&ClientConfig::default()),
            on_human_signature: None,
            server_key: None,
        })
    }

    pub async fn connect_with_config(
        descriptor: &VerifiedEndpointDescriptor,
        config: &ClientConfig,
    ) -> Result<Self> {
        Ok(Self {
            connection: HostedConnection::connect_verified(descriptor).await?,
            context: CallContextFactory::from_client_config(config)?,
            transport: helpers::HostedTransportPolicy::from_client_config(config),
            on_human_signature: None,
            server_key: config.server_key.clone(),
        })
    }

    /// Direct-address constructor for conformance tests and explicit local endpoints.
    pub async fn connect_addr(endpoint: Endpoint, address: EndpointAddr) -> Result<Self> {
        Ok(Self {
            connection: HostedConnection::connect(endpoint, address).await?,
            context: CallContextFactory::default(),
            transport: helpers::HostedTransportPolicy::from_client_config(&ClientConfig::default()),
            on_human_signature: None,
            server_key: None,
        })
    }

    pub async fn connect_addr_with_config(
        endpoint: Endpoint,
        address: EndpointAddr,
        config: &ClientConfig,
    ) -> Result<Self> {
        Ok(Self {
            connection: HostedConnection::connect(endpoint, address).await?,
            context: CallContextFactory::from_client_config(config)?,
            transport: helpers::HostedTransportPolicy::from_client_config(config),
            on_human_signature: None,
            server_key: config.server_key.clone(),
        })
    }

    pub async fn connect_addr_with_context(
        endpoint: Endpoint,
        address: EndpointAddr,
        context: CallContextFactory,
    ) -> Result<Self> {
        Ok(Self {
            connection: HostedConnection::connect(endpoint, address).await?,
            context,
            transport: helpers::HostedTransportPolicy::from_client_config(&ClientConfig::default()),
            on_human_signature: None,
            server_key: None,
        })
    }

    pub fn with_human_signature_callback(mut self, callback: HumanSignatureCallback) -> Self {
        self.on_human_signature = Some(callback);
        self
    }

    pub(super) async fn auto_rotate_if_needed(
        &mut self,
        renewable: Option<&RenewableAuthorityCredential>,
    ) {
        let (Some(renewable), Some(server_key)) = (renewable, self.server_key.clone()) else {
            return;
        };
        let credential = match crate::credentials::resolve_credential_for_server(&server_key) {
            Ok(Some(credential)) => credential,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!("credential rotation: failed to load credential: {error}");
                return;
            }
        };
        if !renewable.matches_active_client(&credential, self.context.bearer_capability())
            || !crate::credentials::token_needs_rotation(&credential)
        {
            return;
        }
        let (Some(public_key_id), Some(private_key_pem)) = (
            credential.credential_id.clone(),
            credential.private_key_pem.clone(),
        ) else {
            tracing::debug!("credential rotation: stored authority has no renewal key");
            return;
        };
        let signer = match Ed25519Signer::from_pem(&private_key_pem) {
            Ok(signer) => signer,
            Err(error) => {
                tracing::warn!("credential rotation: failed to load signing key: {error}");
                return;
            }
        };
        let timestamp = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(duration) => duration.as_secs(),
            Err(error) => {
                tracing::warn!("credential rotation: clock skew: {error}");
                return;
            }
        };
        let signature = match signer.sign(format!("{timestamp}\n{public_key_id}\n").as_bytes()) {
            Ok(signature) => signature,
            Err(error) => {
                tracing::warn!("credential rotation: failed to sign challenge: {error}");
                return;
            }
        };
        let request = api::heddle::api::v1alpha1::MintBiscuitRequest {
            subject: credential.subject.clone(),
            requested_scope: String::new(),
            user_agent: String::new(),
            ip: String::new(),
            proof: Some(
                api::heddle::api::v1alpha1::mint_biscuit_request::Proof::Keypair(
                    api::heddle::api::v1alpha1::KeypairProof {
                        public_key_id,
                        timestamp,
                        signature,
                    },
                ),
            ),
            client_operation_id: String::new(),
        };
        let response = match self.routes().mint_biscuit(&request).await {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!("credential rotation: MintBiscuit failed: {error}");
                return;
            }
        };
        let expires_at = response
            .expires_at
            .as_ref()
            .and_then(|timestamp| chrono::DateTime::from_timestamp(timestamp.seconds.max(0), 0))
            .map(|timestamp| timestamp.to_rfc3339())
            .or(credential.expires_at.clone());
        let updated = crate::credentials::ServerCredential {
            token: response.token.clone(),
            subject: if response.subject.is_empty() {
                credential.subject
            } else {
                response.subject
            },
            device_id: credential.device_id,
            credential_id: credential.credential_id,
            private_key_pem: Some(private_key_pem),
            expires_at,
        };
        if let Err(error) = crate::credentials::store_server_credential(&server_key, updated) {
            tracing::warn!("credential rotation: failed to persist credential: {error}");
        }
        self.context
            .set_bearer_capability(response.token.into_bytes());
    }

    pub async fn call_unary<Request, Response>(
        &self,
        method: &str,
        request: &Request,
    ) -> Result<Response>
    where
        Request: Message,
        Response: Message + Default,
    {
        let encoded = request.encode_to_vec();
        let descriptor = api::method_descriptor(method)
            .ok_or_else(|| HostedError::Framing(format!("unknown hosted method {method}")))?;
        let client_operation_id = descriptor
            .client_operation_id(&encoded)?
            .unwrap_or_default();
        if descriptor.client_operation_id_required && client_operation_id.is_empty() {
            return Err(HostedError::MissingClientOperationId);
        }
        let signed = self.context.unary(method, &encoded, client_operation_id)?;
        match call::unary_encoded(&self.connection, method, &signed.context, &encoded).await {
            Ok(response) => Ok(response),
            Err(HostedError::Call {
                code: api::heddle::api::v1alpha1::CallFailureCode::Unauthenticated,
                message,
                details,
            }) if api::human_verification_challenge(&details).is_some() => {
                let challenge = api::human_verification_challenge(&details)
                    .expect("guarded human-verification detail");
                let canonical = signed
                    .canonical()
                    .ok_or(HostedError::SigningIdentityRequired)?;
                let callback =
                    self.on_human_signature
                        .as_ref()
                        .ok_or_else(|| HostedError::Call {
                            code: api::heddle::api::v1alpha1::CallFailureCode::Unauthenticated,
                            message,
                            details,
                        })?;
                let assertion = callback(HumanSignatureRequest {
                    method_path: method.to_string(),
                    action_summary: format!("Authorize {method}"),
                    challenge: human::challenge(canonical),
                    canonical: canonical.to_vec(),
                    action_url: (!challenge.action_url.is_empty()).then_some(challenge.action_url),
                })
                .map_err(|error| HostedError::Call {
                    code: api::heddle::api::v1alpha1::CallFailureCode::PermissionDenied,
                    message: error.to_string(),
                    details: Vec::new(),
                })?;
                let context = signed.with_human_verification(
                    assertion.signature,
                    api::heddle::api::v1alpha1::HumanVerification {
                        client_data_json: assertion.client_data_json,
                        authenticator_data: assertion.authenticator_data,
                        user_handle: assertion.user_handle.unwrap_or_default(),
                    },
                )?;
                call::unary_encoded(&self.connection, method, &context, &encoded).await
            }
            Err(error) => Err(error),
        }
    }

    pub async fn call_server_stream<Request, Response>(
        &self,
        method: &str,
        request: &Request,
        client_operation_id: impl Into<String>,
    ) -> Result<ServerStream<Response>>
    where
        Request: Message,
        Response: Message + Default,
    {
        let context = self.context.streaming(method, client_operation_id)?;
        call::server_stream(self.connection.clone(), method, &context, request).await
    }

    pub async fn call_bidirectional<Request, Response>(
        &self,
        method: &str,
        client_operation_id: impl Into<String>,
    ) -> Result<BidirectionalStream<Request, Response>>
    where
        Request: Message,
        Response: Message + Default,
    {
        let context = self.context.streaming(method, client_operation_id)?;
        call::bidirectional(self.connection.clone(), method, &context).await
    }

    pub async fn unary<Request, Response>(
        &self,
        method: &str,
        context: &CallContext,
        request: &Request,
    ) -> Result<Response>
    where
        Request: Message,
        Response: Message + Default,
    {
        call::unary(&self.connection, method, context, request).await
    }

    pub async fn server_stream<Request, Response>(
        &self,
        method: &str,
        context: &CallContext,
        request: &Request,
    ) -> Result<ServerStream<Response>>
    where
        Request: Message,
        Response: Message + Default,
    {
        call::server_stream(self.connection.clone(), method, context, request).await
    }

    pub async fn bidirectional<Request, Response>(
        &self,
        method: &str,
        context: &CallContext,
    ) -> Result<BidirectionalStream<Request, Response>>
    where
        Request: Message,
        Response: Message + Default,
    {
        call::bidirectional(self.connection.clone(), method, context).await
    }
}
