//! Hosted gRPC client for the transport rewrite.

mod collaboration;
mod content;
pub(crate) mod helpers;
mod hydration;
pub mod monorepo;
pub(crate) mod operation_id;
pub mod request_signing;
mod session;
mod sync;
mod user;

use cli_shared::{ClientConfig, cleartext_connect_allowed, cleartext_refused_message};
use crypto::{Ed25519Signer, Signer};
use grpc::heddle::api::v1alpha1::{
    KeypairProof, MintBiscuitRequest,
    collaboration_service_client::CollaborationServiceClient,
    identity_service_client::IdentityServiceClient, mint_biscuit_request::Proof,
    registry_service_client::RegistryServiceClient,
    repo_sync_service_client::RepoSyncServiceClient,
    repository_service_client::RepositoryServiceClient,
    workflow_service_client::WorkflowServiceClient,
};
use objects::{object::MarkerName, store::ObjectStore};
use repo::Repository;
use tonic::{
    Request,
    metadata::MetadataValue,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint},
};
use wire::ProtocolError;

use crate::credentials;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RenewableAuthorityCredential {
    token: String,
    credential_id: String,
}

impl RenewableAuthorityCredential {
    pub(super) fn from_stored(credential: &credentials::ServerCredential) -> Option<Self> {
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
        credential: &credentials::ServerCredential,
        token_header: Option<&MetadataValue<tonic::metadata::Ascii>>,
    ) -> bool {
        credential.token == self.token
            && credential.credential_id.as_deref() == Some(self.credential_id.as_str())
            && token_header
                .and_then(|header| header.to_str().ok())
                .and_then(|header| header.strip_prefix("Bearer "))
                == Some(self.token.as_str())
    }
}

pub struct HostedGrpcClient {
    pub(super) inner: RepoSyncServiceClient<Channel>,
    pub(super) user: RegistryServiceClient<Channel>,
    pub(super) auth: IdentityServiceClient<Channel>,
    pub(super) content: RepositoryServiceClient<Channel>,
    pub(super) workflow: WorkflowServiceClient<Channel>,
    pub(super) collaboration: CollaborationServiceClient<Channel>,
    pub(super) token_header: Option<MetadataValue<tonic::metadata::Ascii>>,
    transport: helpers::HostedTransportPolicy,
    pub(super) auth_proof_key_pem: Option<String>,
    authenticated_principal: Option<String>,
    /// The key used to look up this server's credential in the credential
    /// store. When the session also carries an exact renewable-authority
    /// binding, `auto_rotate_if_needed` uses this key to re-read and update
    /// `~/.heddle/credentials.toml` transparently.
    server_key: Option<String>,
    /// App-registered WebAuthn signer invoked when a `human`-tier RPC is
    /// rejected with `x-heddle-sig-required: human`. `None` => human-tier RPCs
    /// surface a typed error rather than looping. See
    /// [`request_signing::HumanSignatureCallback`].
    on_human_signature: Option<request_signing::HumanSignatureCallback>,
}

impl HostedGrpcClient {
    pub async fn connect(
        addr: std::net::SocketAddr,
        config: &ClientConfig,
    ) -> Result<Self, ProtocolError> {
        // Production remotes require TLS. Cleartext is allowed only for
        // loopback or when the user explicitly opts in (`allow_insecure` /
        // `--insecure` / remote.insecure / HEDDLE_REMOTE_INSECURE).
        if !cleartext_connect_allowed(addr, config.tls_enabled, config.allow_insecure) {
            return Err(ProtocolError::InvalidState(cleartext_refused_message(addr)));
        }
        let scheme = if config.tls_enabled { "https" } else { "http" };
        let mut endpoint = Endpoint::from_shared(format!("{scheme}://{addr}"))
            .map_err(|err| ProtocolError::InvalidState(err.to_string()))?;
        if config.tls_enabled {
            let mut tls = ClientTlsConfig::new();
            if let Some(domain_name) = &config.tls_domain_name {
                tls = tls.domain_name(domain_name.clone());
            }
            if let Some(ca_pem) = &config.tls_ca_certificate_pem {
                tls = tls.ca_certificate(Certificate::from_pem(ca_pem.as_bytes()));
            }
            endpoint = endpoint
                .tls_config(tls)
                .map_err(|err| ProtocolError::InvalidState(err.to_string()))?;
        }
        let channel = endpoint
            .connect()
            .await
            .map_err(|err| ProtocolError::Io(std::io::Error::other(err.to_string())))?;
        Self::from_channel(channel, config)
    }

    pub(super) fn from_channel(
        channel: Channel,
        config: &ClientConfig,
    ) -> Result<Self, ProtocolError> {
        let token_header = config
            .token
            .as_ref()
            .map(|token| MetadataValue::try_from(format!("Bearer {}", token.id)))
            .transpose()
            .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
        let transport = helpers::HostedTransportPolicy::from_client_config(config);
        if config.auth_proof_key_pem.is_some() && config.authenticated_principal.is_none() {
            return Err(ProtocolError::AuthenticationFailed(
                "hosted request signing requires an authenticated principal".to_string(),
            ));
        }
        Ok(Self {
            // Bound the single-shot, server-controlled sidecar allocation at
            // the gRPC decode boundary: tonic rejects an oversized inbound
            // `PullMessage` before its `redactions_blob`/`state_visibility_blob`
            // `Vec<u8>` is ever materialized. The post-decode
            // `check_received_transfer_blob_size` calls are kept as cheap
            // defense-in-depth, but this is the load-bearing guard.
            inner: RepoSyncServiceClient::new(channel.clone())
                .max_decoding_message_size(wire::MAX_PULL_DECODE_MESSAGE_SIZE),
            user: RegistryServiceClient::new(channel.clone()),
            auth: IdentityServiceClient::new(channel.clone()),
            content: RepositoryServiceClient::new(channel.clone()),
            workflow: WorkflowServiceClient::new(channel.clone()),
            collaboration: CollaborationServiceClient::new(channel.clone()),
            token_header,
            transport,
            auth_proof_key_pem: config.auth_proof_key_pem.clone(),
            authenticated_principal: config.authenticated_principal.clone(),
            server_key: config.server_key.clone(),
            on_human_signature: None,
        })
    }

    /// Register the app's WebAuthn signer for the destructive (`human`) tier.
    ///
    /// Invoked when a signed RPC is rejected with `x-heddle-sig-required: human`;
    /// the callback produces a [`request_signing::WebAuthnAssertion`] over the
    /// same action and the call is retried once. With no callback registered, a
    /// human-tier rejection surfaces a typed error (no loop). The CLI wires a
    /// terminal-prompt implementation; tapestry a browser ceremony.
    pub fn with_human_signature_callback(
        mut self,
        callback: request_signing::HumanSignatureCallback,
    ) -> Self {
        self.on_human_signature = Some(callback);
        self
    }

    /// The device signer for request PoP, derived from the same
    /// `auth_proof_key_pem` the client uses for the `x-heddle-proof` bearer
    /// proof-of-possession. `None` when the client is anonymous / unauthed —
    /// signing is then skipped (the server defaults to OBSERVE mode and ignores
    /// missing signatures on unsigned-tier RPCs).
    fn device_signer(&self) -> Result<Option<Ed25519Signer>, ProtocolError> {
        match &self.auth_proof_key_pem {
            Some(pem) => Ed25519Signer::from_pem(pem)
                .map(Some)
                .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string())),
            None => Ok(None),
        }
    }

    fn stable_signing_identity(&self) -> Result<&str, ProtocolError> {
        self.authenticated_principal
            .as_deref()
            .filter(|principal| {
                principal
                    .strip_prefix("principal:")
                    .is_some_and(|subject| !subject.trim().is_empty())
            })
            .ok_or_else(|| {
                ProtocolError::AuthenticationFailed(
                    "hosted request signing requires the bearer token's stable authenticated principal"
                        .to_string(),
                )
            })
    }

    /// Stamp bearer auth (token + `x-heddle-proof`) and, for unary requests,
    /// attach the Tier-1 PoP request signature over the serialized body.
    ///
    /// This is the single chokepoint every UNARY authenticated call routes
    /// through. Streaming call sites (which have no single body to hash) call
    /// [`Self::apply_auth`] directly instead. Returns the signing context so a
    /// human-tier retry can re-derive the identical WebAuthn challenge; `None`
    /// when signing was skipped (anonymous client).
    pub(in crate::grpc_hosted) fn apply_signed_auth<T: prost::Message>(
        &self,
        request: &mut Request<T>,
        method_path: &str,
    ) -> Result<Option<request_signing::SignedRequestContext>, ProtocolError> {
        self.apply_auth(request, method_path)?;
        let Some(signer) = self.device_signer()? else {
            return Ok(None);
        };
        let message_bytes = request.get_ref().encode_to_vec();
        let identity = self.stable_signing_identity()?;
        let ctx =
            request_signing::attach_pop(request, &signer, identity, method_path, &message_bytes)?;
        Ok(Some(ctx))
    }

    /// A human-tier rejection can only be satisfied if the original request was
    /// PoP-signed (so we have a `SignedRequestContext` to derive the WebAuthn
    /// challenge from). An anonymous client (no device key) that somehow reaches
    /// a human-tier RPC has no context — surface a typed error, don't loop.
    pub(in crate::grpc_hosted) fn require_human_sig_context(
        &self,
        ctx: Option<request_signing::SignedRequestContext>,
    ) -> Result<request_signing::SignedRequestContext, ProtocolError> {
        ctx.ok_or_else(|| {
            ProtocolError::AuthorizationFailed(
                "this action requires user verification, but the client has no device key to \
                 sign the request; run `heddle auth login` first"
                    .to_string(),
            )
        })
    }

    /// Invoke the app-registered human-signature callback over the pending
    /// action. The WebAuthn challenge is client-derived
    /// (`SHA256(canonical bytes)`) — no server round trip. If no callback is
    /// registered, surface a typed error rather than looping.
    pub(in crate::grpc_hosted) fn request_human_signature(
        &self,
        method_path: &str,
        ctx: &request_signing::SignedRequestContext,
        action_url: Option<String>,
    ) -> Result<request_signing::WebAuthnAssertion, ProtocolError> {
        let callback = self.on_human_signature.as_ref().ok_or_else(|| {
            ProtocolError::AuthorizationFailed(format!(
                "action {method_path} requires user verification, but no WebAuthn signer is \
                 configured for this client"
            ))
        })?;
        let challenge = request_signing::human_challenge(&ctx.canonical);
        let req = request_signing::HumanSignatureRequest {
            method_path: method_path.to_string(),
            action_summary: format!("Authorize {method_path}"),
            challenge,
            canonical: ctx.canonical.clone(),
            // Deep-link the server sent on the rejection (weft#338), if any — a display hint
            // the callback can show; the signed challenge above is unaffected.
            action_url,
        };
        callback(req)
    }

    pub(super) fn apply_auth<T>(
        &self,
        request: &mut Request<T>,
        method_path: &str,
    ) -> Result<(), ProtocolError> {
        if let Some(token) = &self.token_header {
            request
                .metadata_mut()
                .insert("authorization", token.clone());
            if let Some(pem) = &self.auth_proof_key_pem {
                let signer = Ed25519Signer::from_pem(pem)
                    .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
                let raw = token
                    .to_str()
                    .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
                let bearer = raw
                    .strip_prefix("Bearer ")
                    .or_else(|| raw.strip_prefix("bearer "))
                    .unwrap_or(raw);
                let proof_ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?
                    .as_secs()
                    .to_string();
                let mut nonce_bytes = [0u8; 16];
                rand::fill(&mut nonce_bytes);
                let nonce = hex::encode(nonce_bytes);
                let signature =
                    crypto::pop::sign_pop(&signer, bearer, &proof_ts, "POST", method_path, &nonce)
                        .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
                use base64::Engine;
                let encoded = base64::engine::general_purpose::STANDARD.encode(signature);
                let proof = MetadataValue::try_from(encoded)
                    .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
                request.metadata_mut().insert(crypto::pop::HDR_PROOF, proof);
                let proof_ts = MetadataValue::try_from(proof_ts)
                    .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
                request
                    .metadata_mut()
                    .insert(crypto::pop::HDR_PROOF_TS, proof_ts);
                let nonce = MetadataValue::try_from(nonce)
                    .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
                request
                    .metadata_mut()
                    .insert(crypto::pop::HDR_PROOF_NONCE, nonce);
            }
        }
        Ok(())
    }

    /// Transparently rotate the credential for this client if it is near expiry.
    ///
    /// No-ops unless session construction proved that the active bearer is the
    /// exact authority credential loaded from this server's credential-store
    /// row. Explicit config/env tokens and attenuated tokens never enter this
    /// renewal path.
    pub(super) async fn auto_rotate_if_needed(
        &mut self,
        renewable: Option<&RenewableAuthorityCredential>,
    ) {
        let Some(renewable) = renewable else {
            return;
        };
        let server_key = match &self.server_key {
            Some(k) => k.clone(),
            None => return,
        };
        self.rotate_credential_for_server(&server_key, renewable)
            .await;
    }

    async fn rotate_credential_for_server(
        &mut self,
        server_key: &str,
        renewable: &RenewableAuthorityCredential,
    ) {
        // Load the stored credential.
        let cred = match credentials::resolve_credential_for_server(server_key) {
            Ok(Some(c)) => c,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!("credential rotation: failed to load credential: {err}");
                return;
            }
        };

        // The store may have changed after session construction. Renewal is
        // permitted only while both the active bearer and the current row are
        // still the exact token + credential id that were selected together.
        if !renewable.matches_active_client(&cred, self.token_header.as_ref()) {
            tracing::debug!(
                "credential rotation: active bearer no longer matches the selected stored credential"
            );
            return;
        }

        // Check whether the Biscuit's stored expiry is within the
        // rotation window.
        if !credentials::token_needs_rotation(&cred) {
            return;
        }

        // We need both `credential_id` (the public key id the server
        // will look up) and `private_key_pem` (to sign the renewal
        // proof). Older credentials without one or the other can't
        // self-renew; the user falls back to `heddle auth login`.
        let public_key_id = match &cred.credential_id {
            Some(id) => id.clone(),
            None => {
                tracing::debug!("credential rotation: no credential_id stored, skipping");
                return;
            }
        };
        let private_key_pem = match &cred.private_key_pem {
            Some(pem) => pem.clone(),
            None => {
                tracing::debug!("credential rotation: no private_key_pem stored, skipping");
                return;
            }
        };

        // Sign the canonical renewal challenge:
        //   "{timestamp}\n{public_key_id}\n{requested_scope}"
        // Empty `requested_scope` == reuse the keypair owner's
        // original scope. The server clamps anyway, so a permissive
        // hint is fine.
        let signer = match Ed25519Signer::from_pem(&private_key_pem) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!("credential rotation: failed to load signing key: {err}");
                return;
            }
        };
        let timestamp = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs(),
            Err(err) => {
                tracing::warn!("credential rotation: clock skew: {err}");
                return;
            }
        };
        let canonical = format!("{timestamp}\n{public_key_id}\n");
        let signature = match signer.sign(canonical.as_bytes()) {
            Ok(sig) => sig,
            Err(err) => {
                tracing::warn!("credential rotation: failed to sign challenge: {err}");
                return;
            }
        };

        let mut request = Request::new(MintBiscuitRequest {
            subject: cred.subject.clone(),
            requested_scope: String::new(),
            user_agent: String::new(),
            ip: String::new(),
            proof: Some(Proof::Keypair(KeypairProof {
                public_key_id,
                timestamp,
                signature,
            })),
            client_operation_id: String::new(),
        });
        // MintBiscuit is unauthenticated — the proof is the auth.
        // We deliberately skip `apply_auth` here.
        let _ = &mut request;

        let response = match self.auth.mint_biscuit(request).await {
            Ok(r) => r.into_inner(),
            Err(status) => {
                tracing::warn!(
                    "credential rotation: MintBiscuit failed: {} — continuing with existing token",
                    status.message()
                );
                return;
            }
        };

        // Format the new expiry as RFC 3339.
        let expires_at_secs = response
            .expires_at
            .as_ref()
            .map(|t| t.seconds.max(0))
            .unwrap_or(0);
        let new_expires_at = if expires_at_secs > 0 {
            chrono::DateTime::from_timestamp(expires_at_secs, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| expires_at_secs.to_string())
        } else {
            String::new()
        };

        tracing::debug!(
            "credential rotation: rotated successfully, new expiry: {}",
            new_expires_at
        );

        // Persist the updated credential. The keypair stays the
        // same — that's the whole point of the keypair-based renewal
        // model. We replace `token` (the Biscuit) and bump
        // `expires_at` to the fresh window.
        let updated = credentials::ServerCredential {
            token: response.token.clone(),
            subject: if response.subject.is_empty() {
                cred.subject.clone()
            } else {
                response.subject
            },
            device_id: cred.device_id.clone(),
            credential_id: cred.credential_id.clone(),
            private_key_pem: Some(private_key_pem),
            expires_at: if new_expires_at.is_empty() {
                cred.expires_at.clone()
            } else {
                Some(new_expires_at)
            },
        };

        if let Err(err) = credentials::store_server_credential(server_key, updated) {
            tracing::warn!("credential rotation: failed to persist updated credential: {err}");
            // Don't bail — the in-memory update below still improves the session.
        }

        // Update the in-memory token header so the remaining RPCs on this
        // client instance use the fresh token.
        match MetadataValue::try_from(format!("Bearer {}", response.token)) {
            Ok(header) => self.token_header = Some(header),
            Err(err) => {
                tracing::warn!("credential rotation: failed to set new token header: {err}");
            }
        }
    }

    pub(super) async fn sync_remote_markers(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        pushed_state: objects::object::StateId,
    ) -> Result<(), ProtocolError> {
        let remote_markers = self
            .list_refs(repo_path)
            .await?
            .into_iter()
            .filter(|entry| !entry.is_thread)
            .map(|entry| (entry.name, entry.state_id))
            .collect::<std::collections::HashMap<_, _>>();
        for marker in repo.refs().list_markers()? {
            let Some(state_id) = repo.refs().get_marker(&marker)? else {
                continue;
            };
            if !wire::is_ancestor(repo.store(), state_id, pushed_state)? {
                continue;
            }

            let old_value = remote_markers.get(marker.as_str()).copied();
            if old_value == Some(state_id) {
                continue;
            }

            let result = self
                .update_ref(
                    repo_path,
                    &marker,
                    false,
                    old_value,
                    state_id,
                    true,
                    None,
                    operation_id::ClientOperationId::fresh(
                        "heddle.api.v1alpha1.RepoSyncService/UpdateRef",
                    )
                    .to_wire(),
                )
                .await?;
            if !result.success {
                return Err(ProtocolError::InvalidState(
                    result
                        .error
                        .unwrap_or_else(|| format!("failed to sync marker '{marker}'")),
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn sync_local_markers(
        &mut self,
        repo: &Repository,
        repo_path: &str,
    ) -> Result<(), ProtocolError> {
        let remote_markers = self.list_refs(repo_path).await?;
        for marker in remote_markers.into_iter().filter(|entry| !entry.is_thread) {
            if !repo.store().has_state(&marker.state_id)? {
                continue;
            }
            let marker_name = MarkerName::from(marker.name.as_str());
            match repo.refs().get_marker(&marker_name)? {
                Some(existing) if existing == marker.state_id => {}
                Some(existing) => repo.refs().set_marker_cas(
                    &marker_name,
                    refs::RefExpectation::Value(existing),
                    &marker.state_id,
                )?,
                None => repo.refs().create_marker(&marker_name, &marker.state_id)?,
            }
        }
        Ok(())
    }
}

pub use collaboration::{HostedDiscussion, HostedDiscussionTurn};
pub use hydration::{LazyHostedHydrator, PullMaterialization, register_hosted_factory};
pub use monorepo::{MonorepoCloneOp, MonorepoClonePlan, SkippedChild};
pub use session::{HostedAuthMode, HostedSession};
pub use sync::HostedRefEntry;

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use biscuit_auth::{Biscuit, KeyPair};

    use super::*;

    fn test_client(auth_proof_key_pem: String, token: &str) -> HostedGrpcClient {
        let channel = Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
        let config = ClientConfig::default();
        HostedGrpcClient {
            inner: RepoSyncServiceClient::new(channel.clone())
                .max_decoding_message_size(wire::MAX_PULL_DECODE_MESSAGE_SIZE),
            user: RegistryServiceClient::new(channel.clone()),
            auth: IdentityServiceClient::new(channel.clone()),
            content: RepositoryServiceClient::new(channel.clone()),
            workflow: WorkflowServiceClient::new(channel.clone()),
            collaboration: CollaborationServiceClient::new(channel.clone()),
            token_header: Some(
                MetadataValue::try_from(format!("Bearer {token}")).expect("valid bearer header"),
            ),
            transport: helpers::HostedTransportPolicy::from_client_config(&config),
            auth_proof_key_pem: Some(auth_proof_key_pem),
            authenticated_principal: Some("principal:alice".to_string()),
            server_key: None,
            on_human_signature: None,
        }
    }

    #[tokio::test]
    async fn signed_client_fails_closed_without_an_authenticated_principal() {
        let signer = Ed25519Signer::generate().expect("proof signer");
        let config =
            ClientConfig::default().with_auth_proof_key_pem(signer.to_pem().expect("proof PEM"));
        let channel = Endpoint::from_static("http://127.0.0.1:1").connect_lazy();

        let error = match HostedGrpcClient::from_channel(channel, &config) {
            Ok(_) => panic!("proof signing without a principal must fail closed"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("authenticated principal"));
    }

    #[test]
    fn renewable_authority_binding_requires_exact_token_and_credential_id() {
        let signer = Ed25519Signer::generate().expect("proof signer");
        let token = Biscuit::builder()
            .fact(r#"user("alice")"#)
            .expect("user fact")
            .fact(r#"credential_id("cred-1")"#)
            .expect("credential fact")
            .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
            .expect("proof key fact")
            .build(&KeyPair::new())
            .expect("mint authority token")
            .to_base64()
            .expect("encode authority token");
        let credential = credentials::ServerCredential {
            token: token.clone(),
            subject: "alice".to_string(),
            device_id: Some("device-1".to_string()),
            credential_id: Some("cred-1".to_string()),
            private_key_pem: Some(signer.to_pem().expect("proof PEM")),
            expires_at: None,
        };
        let binding = RenewableAuthorityCredential::from_stored(&credential)
            .expect("valid stored authority credential");
        let mut mismatched_claim = credential.clone();
        mismatched_claim.credential_id = Some("cred-2".to_string());
        assert!(
            RenewableAuthorityCredential::from_stored(&mismatched_claim).is_none(),
            "the stored credential id must match the authority token claim"
        );
        let attenuated_token = biscuit_auth::UnverifiedBiscuit::from_base64(token.as_bytes())
            .expect("parse authority token")
            .append(
                biscuit_auth::builder::BlockBuilder::new()
                    .fact(r#"agent("child")"#)
                    .expect("child fact"),
            )
            .expect("append child block")
            .to_base64()
            .expect("encode attenuated token");
        let mut attenuated_credential = credential.clone();
        attenuated_credential.token = attenuated_token;
        assert!(
            RenewableAuthorityCredential::from_stored(&attenuated_credential).is_none(),
            "an attenuated token is never an authority-renewal source"
        );
        let matching_header =
            MetadataValue::try_from(format!("Bearer {token}")).expect("matching bearer header");
        assert!(binding.matches_active_client(&credential, Some(&matching_header)));

        let wrong_header =
            MetadataValue::try_from("Bearer explicit-child").expect("different bearer header");
        assert!(!binding.matches_active_client(&credential, Some(&wrong_header)));

        let mut wrong_id = credential.clone();
        wrong_id.credential_id = Some("cred-2".to_string());
        assert!(!binding.matches_active_client(&wrong_id, Some(&matching_header)));

        let mut wrong_token = credential;
        wrong_token.token = "different-stored-token".to_string();
        assert!(!binding.matches_active_client(&wrong_token, Some(&matching_header)));
    }

    #[tokio::test]
    async fn apply_auth_attaches_verifiable_v2_pop_proof() {
        let signer = Ed25519Signer::generate().expect("generate signer");
        let pem = signer.to_pem().expect("export signer pem");
        let token = "test-token";
        let path = "/heddle.api.v1alpha1.IdentityService/WhoAmI";
        let client = test_client(pem, token);
        let mut request = Request::new(());

        client.apply_auth(&mut request, path).expect("apply auth");

        let md = request.metadata();
        let proof_ts = md
            .get(crypto::pop::HDR_PROOF_TS)
            .and_then(|v| v.to_str().ok())
            .expect("proof timestamp header");
        let nonce = md
            .get(crypto::pop::HDR_PROOF_NONCE)
            .and_then(|v| v.to_str().ok())
            .expect("proof nonce header");
        assert!(!nonce.is_empty());
        assert!(nonce.len() <= 256);

        let proof = md
            .get(crypto::pop::HDR_PROOF)
            .and_then(|v| v.to_str().ok())
            .expect("proof header");
        let sig = base64::engine::general_purpose::STANDARD
            .decode(proof)
            .expect("proof decodes");
        let canonical = crypto::pop::pop_canonical_payload(token, proof_ts, "POST", path, nonce);
        crypto::verify_payload_signature(&canonical, "ed25519", signer.public_key(), &sig)
            .expect("proof verifies against device public key");
    }

    #[tokio::test]
    async fn service_account_issue_preserves_custom_proof_and_adds_full_hosted_auth_chain() {
        use grpc::heddle::api::v1alpha1::IssueServiceAccountCredentialRequest;

        let signer = Ed25519Signer::generate().expect("generate signer");
        let pem = signer.to_pem().expect("export signer pem");
        let client = test_client(pem, "stored-biscuit");
        let mut request = Request::new(IssueServiceAccountCredentialRequest {
            service_account_id: "sa-1".to_string(),
            public_key: vec![7; 32],
            scope: "repo:acme/*".to_string(),
            ttl_secs: None,
            client_operation_id: "stable-op-1".to_string(),
        });
        request
            .metadata_mut()
            .insert("x-heddle-issue-sa-proof-ts", "1700000000".parse().unwrap());
        request.metadata_mut().insert_bin(
            "x-heddle-issue-sa-proof-sig-bin",
            tonic::metadata::MetadataValue::from_bytes(b"service-account-proof"),
        );

        client
            .apply_signed_auth(
                &mut request,
                "/heddle.api.v1alpha1.IdentityService/IssueServiceAccountCredential",
            )
            .expect("prepare signed request");
        let metadata = request.metadata();
        assert!(metadata.get("authorization").is_some());
        assert!(metadata.get(crypto::pop::HDR_PROOF).is_some());
        assert!(metadata.get(crypto::pop::HDR_PROOF_TS).is_some());
        assert!(metadata.get(crypto::pop::HDR_PROOF_NONCE).is_some());
        assert!(metadata.get(request_signing::HDR_SIG_TS).is_some());
        assert!(metadata.get_bin(request_signing::HDR_SIG_BIN).is_some());
        assert!(
            metadata
                .get_bin(request_signing::HDR_SIG_NONCE_BIN)
                .is_some()
        );
        assert_eq!(
            metadata
                .get("x-heddle-issue-sa-proof-ts")
                .and_then(|value| value.to_str().ok()),
            Some("1700000000")
        );
        assert!(
            metadata
                .get_bin("x-heddle-issue-sa-proof-sig-bin")
                .is_some()
        );
        assert_eq!(request.get_ref().client_operation_id, "stable-op-1");
    }
}
