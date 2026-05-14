//! Hosted gRPC client for the transport rewrite.

mod content;
mod helpers;
mod hydration;
mod sync;
mod user;

use crypto::{Ed25519Signer, Signer};
use grpc::heddle::v1::{
    KeypairProof, MintBiscuitRequest, auth_service_client::AuthServiceClient,
    content_service_client::ContentServiceClient,
    hosted_user_service_client::HostedUserServiceClient, mint_biscuit_request::Proof,
    repo_sync_service_client::RepoSyncServiceClient,
};
use proto::ProtocolError;
use repo::Repository;
use tonic::{
    Request,
    metadata::MetadataValue,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint},
};

use crate::credentials;
use cli_shared::ClientConfig;

pub struct HostedGrpcClient {
    pub(super) inner: RepoSyncServiceClient<Channel>,
    pub(super) user: HostedUserServiceClient<Channel>,
    pub(super) auth: AuthServiceClient<Channel>,
    pub(super) content: ContentServiceClient<Channel>,
    pub(super) token_header: Option<MetadataValue<tonic::metadata::Ascii>>,
    transport: helpers::HostedTransportPolicy,
    pub(super) auth_proof_key_pem: Option<String>,
    /// The key used to look up this server's credential in the credential
    /// store.  When set, `auto_rotate_if_needed` will use it to read and
    /// update `~/.heddle/credentials.toml` transparently.
    server_key: Option<String>,
}

impl HostedGrpcClient {
    pub async fn connect(
        addr: std::net::SocketAddr,
        config: &ClientConfig,
    ) -> Result<Self, ProtocolError> {
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
        let token_header = config
            .token
            .as_ref()
            .map(|token| MetadataValue::try_from(format!("Bearer {}", token.id)))
            .transpose()
            .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
        let transport = helpers::HostedTransportPolicy::from_client_config(config);
        Ok(Self {
            inner: RepoSyncServiceClient::new(channel.clone()),
            user: HostedUserServiceClient::new(channel.clone()),
            auth: AuthServiceClient::new(channel.clone()),
            content: ContentServiceClient::new(channel),
            token_header,
            transport,
            auth_proof_key_pem: config.auth_proof_key_pem.clone(),
            server_key: config.server_key.clone(),
        })
    }

    pub(super) fn apply_auth<T>(&self, request: &mut Request<T>) -> Result<(), ProtocolError> {
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
                let signature = signer
                    .sign(format!("{bearer}|{proof_ts}").as_bytes())
                    .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
                use base64::Engine;
                let encoded = base64::engine::general_purpose::STANDARD.encode(signature);
                let proof = MetadataValue::try_from(encoded)
                    .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
                request.metadata_mut().insert("x-heddle-proof", proof);
                let proof_ts = MetadataValue::try_from(proof_ts)
                    .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
                request.metadata_mut().insert("x-heddle-proof-ts", proof_ts);
            }
        }
        Ok(())
    }

    /// Transparently rotate the credential for this client if it is near expiry.
    ///
    /// No-ops if `server_key` was not set on `ClientConfig` at construction
    /// time, or if no credential is stored for the server, or if the token is
    /// not within 10 minutes of expiry.
    pub async fn auto_rotate_if_needed(&mut self) {
        let server_key = match &self.server_key {
            Some(k) => k.clone(),
            None => return,
        };
        self.rotate_credential_for_server(&server_key).await;
    }

    async fn rotate_credential_for_server(&mut self, server_key: &str) {
        // Load the stored credential.
        let cred = match credentials::resolve_credential_for_server(server_key) {
            Ok(Some(c)) => c,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!("credential rotation: failed to load credential: {err}");
                return;
            }
        };

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
        pushed_state: objects::object::ChangeId,
    ) -> Result<(), ProtocolError> {
        let remote_markers = self
            .list_refs(repo_path)
            .await?
            .into_iter()
            .filter(|entry| !entry.is_thread)
            .map(|entry| (entry.name, entry.change_id))
            .collect::<std::collections::HashMap<_, _>>();
        for marker in repo.refs().list_markers()? {
            let Some(change_id) = repo.refs().get_marker(&marker)? else {
                continue;
            };
            if !proto::is_ancestor(repo.store(), change_id, pushed_state)? {
                continue;
            }

            let old_value = remote_markers.get(&marker).copied();
            if old_value == Some(change_id) {
                continue;
            }

            let result = self
                .update_ref(repo_path, &marker, false, old_value, change_id, true, None)
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
            if !repo.store().has_state(&marker.change_id)? {
                continue;
            }
            match repo.refs().get_marker(&marker.name)? {
                Some(existing) if existing == marker.change_id => {}
                Some(existing) => repo.refs().set_marker_cas(
                    &marker.name,
                    refs::RefExpectation::Value(existing),
                    &marker.change_id,
                )?,
                None => repo.refs().create_marker(&marker.name, &marker.change_id)?,
            }
        }
        Ok(())
    }
}

pub use hydration::PullMaterialization;
