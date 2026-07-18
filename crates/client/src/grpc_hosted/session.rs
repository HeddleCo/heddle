//! Hosted-session open seam.
//!
//! "Open a usable hosted session for a remote" used to be hand-assembled at
//! every hosted entry point (push, pull, fetch, clone, support, approval,
//! lazy hydration): resolve the auth token, fall back to the credential
//! store, attach the proof key, build the validated client config, connect,
//! then rotate only an eligible stored authority credential. This module owns
//! that assembly so the command modules choose only intent + remote and call
//! one seam.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use cli_shared::{ClientConfig, UserConfig};
use crypto::{Ed25519Signer, Signer};
use tonic::transport::Channel;
use wire::{AuthToken, ProtocolError};

use crate::{
    credentials,
    grpc_hosted::{HostedGrpcClient, RenewableAuthorityCredential},
};

/// How a hosted session resolves its auth token.
pub enum HostedAuthMode {
    /// Use only the token from env/user config (`remote_token`). Used by
    /// fetch, support, approval, and lazy hydration.
    ConfigToken,
    /// Use the config token, falling back to the per-server credential store
    /// (and its proof key) when no config token is present. Used by push and
    /// pull.
    CredentialFallback,
}

/// A validated, connectable hosted-session configuration.
///
/// Building runs the fallible TLS/auth validation up front, so callers that
/// must not leave partial on-disk artifacts (clone, push) — or that build the
/// config on one thread and connect on another (lazy hydration) — can
/// prevalidate before any irreversible work, then `connect()` afterwards.
/// Callers with no such ordering constraint use
/// [`HostedGrpcClient::open_session`], which builds and connects in one call.
pub struct HostedSession {
    config: ClientConfig,
    renewable_authority_credential: Option<RenewableAuthorityCredential>,
}

impl HostedSession {
    /// Build a session from the exact stored authority credential selected by
    /// an auth command. Unlike `CredentialFallback`, configured/env tokens
    /// cannot replace the selected bearer. The matching device proof key is
    /// required and validated before the command performs any mutation.
    pub fn build_stored_credential(user_config: &UserConfig, server_key: &str) -> Result<Self> {
        let credential = credentials::get_server_credential(server_key)?.ok_or_else(|| {
            anyhow::anyhow!(weft_client_shim::HostedRecoveryAdvice::auth_required(
                server_key
            ))
        })?;
        let proof_key = validated_stored_proof_key(&credential, server_key)?;
        let authenticated_principal = validated_authenticated_principal(&credential)?;
        let renewable_authority_credential = RenewableAuthorityCredential::from_stored(&credential);
        let token = AuthToken::new(credential.token, "credential-store");
        let mut config = user_config.heddle_client_config(Some(token))?;
        config = config
            .with_server_key(server_key.to_string())
            .with_auth_proof_key_pem(proof_key)
            .with_authenticated_principal(authenticated_principal);
        Ok(Self {
            config,
            renewable_authority_credential,
        })
    }

    /// Resolve auth + build the validated client config for a hosted session.
    /// Owns credential-store fallback (per `mode`), server-key attachment, and
    /// proof-key attachment from either a credential or the matching shared
    /// same-host device identity — the assembly the command modules used to
    /// hand-roll.
    pub fn build(
        user_config: &UserConfig,
        server_key: Option<String>,
        mode: HostedAuthMode,
    ) -> Result<Self> {
        let (
            token,
            mut credential_proof_key,
            renewable_authority_credential,
            stored_credential_subject,
        ) = match mode {
            HostedAuthMode::ConfigToken => (user_config.remote_token()?, None, None, None),
            HostedAuthMode::CredentialFallback => {
                let mut token = user_config.remote_token()?;
                let mut credential_proof_key = None;
                let mut renewable_authority_credential = None;
                let mut stored_credential_subject = None;
                if token.is_none()
                    && let Some(ref key) = server_key
                {
                    // Propagate a malformed credentials.toml instead of
                    // swallowing it: a parse error here (e.g. a missing
                    // `subject` field) used to fall through to an
                    // unauthenticated request, which the server rejects with
                    // the opaque "missing authorization metadata" — hiding the
                    // real cause. The `?` surfaces the underlying
                    // "parsing <path>: <toml error>" so the user can fix the
                    // file. A *missing* file still returns Ok(None) and falls
                    // back cleanly.
                    if let Some(cred) = credentials::resolve_credential_for_server(key)? {
                        renewable_authority_credential =
                            RenewableAuthorityCredential::from_stored(&cred);
                        stored_credential_subject = Some(cred.subject.clone());
                        token = Some(AuthToken::new(cred.token, "credential-store"));
                        credential_proof_key = cred.private_key_pem;
                    }
                }
                (
                    token,
                    credential_proof_key,
                    renewable_authority_credential,
                    stored_credential_subject,
                )
            }
        };

        if credential_proof_key.is_none()
            && let Some(ref key) = server_key
            && let Some(token) = token.as_ref()
        {
            credential_proof_key = shared_device_proof_key(key, &token.id)?;
        }

        let mut config = user_config.heddle_client_config(token)?;
        if let Some(key) = server_key {
            config = config.with_server_key(key);
        }
        if let Some(pem) = credential_proof_key
            && config.auth_proof_key_pem.is_none()
        {
            config = config.with_auth_proof_key_pem(pem);
        }
        if config.auth_proof_key_pem.is_some() {
            let token = config.token.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "hosted request signing has a proof key but no authenticated bearer token"
                )
            })?;
            let subject = crate::device_flow::authenticated_subject(&token.id)
                .context("reading the hosted bearer token's authenticated principal")?;
            if stored_credential_subject
                .as_deref()
                .is_some_and(|stored| stored != subject.as_str())
            {
                anyhow::bail!(
                    "stored credential subject does not match the bearer token's authenticated principal"
                );
            }
            config = config.with_authenticated_principal(format!("principal:{subject}"));
        }
        Ok(Self {
            config,
            renewable_authority_credential,
        })
    }

    /// Explicitly allow cleartext to non-loopback hosts for this session
    /// (CLI `--insecure` or remote `insecure = true`). Does not disable an
    /// allow already set via user config / env.
    pub fn with_allow_insecure(mut self, allow: bool) -> Self {
        if allow {
            self.config.allow_insecure = true;
        }
        self
    }

    /// Connect and evaluate renewal for an eligible stored authority token.
    ///
    /// The eligibility check runs immediately after connect and requires the
    /// active bearer to remain byte-for-byte identical to the stored one-block
    /// authority credential selected during [`HostedSession::build`]. Explicit
    /// config/env and attenuated tokens are never rotated here.
    pub async fn connect(&self, addr: SocketAddr) -> Result<HostedGrpcClient, ProtocolError> {
        let mut client = HostedGrpcClient::connect(addr, &self.config).await?;
        client
            .auto_rotate_if_needed(self.renewable_authority_credential.as_ref())
            .await;
        Ok(client)
    }

    /// Finish a validated session over a channel whose URI/TLS policy was
    /// already established by the auth command's endpoint connector.
    pub async fn connect_channel(
        &self,
        channel: Channel,
    ) -> Result<HostedGrpcClient, ProtocolError> {
        let mut client = HostedGrpcClient::from_channel(channel, &self.config)?;
        client
            .auto_rotate_if_needed(self.renewable_authority_credential.as_ref())
            .await;
        Ok(client)
    }
}

fn validated_authenticated_principal(credential: &credentials::ServerCredential) -> Result<String> {
    let subject = crate::device_flow::authenticated_subject(&credential.token)
        .context("reading the stored credential's authenticated principal")?;
    if subject != credential.subject {
        anyhow::bail!(
            "stored credential subject does not match the bearer token's authenticated principal"
        );
    }
    Ok(format!("principal:{subject}"))
}

fn validated_stored_proof_key(
    credential: &credentials::ServerCredential,
    server_key: &str,
) -> Result<String> {
    let pem = credential.private_key_pem.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "stored credential for {server_key} has no device proof key; run `heddle auth login --server {server_key}` first"
        )
    })?;
    let signer = Ed25519Signer::from_pem(pem)
        .map_err(|error| anyhow::anyhow!("stored device proof key is invalid: {error}"))?;
    let token_key = crate::device_flow::effective_pop_public_key_hex(&credential.token)
        .context("reading the stored credential's effective proof key")?;
    if !token_key.eq_ignore_ascii_case(&hex::encode(signer.public_key())) {
        anyhow::bail!("stored device proof key does not match the credential Biscuit");
    }
    Ok(pem.to_string())
}

fn shared_device_proof_key(server_key: &str, token: &str) -> Result<Option<String>> {
    let identity = repo::identity::load_device(&repo::identity::device_identity_path())
        .context("loading this host's shared device identity")?;
    let Some(identity) = identity else {
        return Ok(None);
    };
    if !server_keys_match(&identity.server, server_key)
        || !token_proof_key_matches(token, &identity.public_key)
    {
        return Ok(None);
    }
    Ok(Some(identity.private_key_pem))
}

fn token_proof_key_matches(token: &str, expected_public_key_hex: &str) -> bool {
    crate::device_flow::effective_pop_public_key_hex(token)
        .is_ok_and(|proof_key| proof_key.eq_ignore_ascii_case(expected_public_key_hex))
}

fn server_keys_match(left: &str, right: &str) -> bool {
    fn without_scheme(value: &str) -> &str {
        value
            .strip_prefix("http://")
            .or_else(|| value.strip_prefix("https://"))
            .or_else(|| value.strip_prefix("heddle://"))
            .unwrap_or(value)
    }

    without_scheme(left) == without_scheme(right)
}

impl HostedGrpcClient {
    /// Open a usable hosted session in one call: resolve auth, build the
    /// validated client config, connect, and evaluate eligible stored-authority
    /// renewal. Callers that must prevalidate the config before irreversible
    /// work build a [`HostedSession`] first, then [`HostedSession::connect`].
    pub async fn open_session(
        addr: SocketAddr,
        user_config: &UserConfig,
        server_key: Option<String>,
        mode: HostedAuthMode,
    ) -> Result<Self> {
        Self::open_session_with_insecure(addr, user_config, server_key, mode, false).await
    }

    /// Like [`open_session`], but allows cleartext to non-loopback when
    /// `allow_insecure` is true (CLI `--insecure` / remote `insecure = true`).
    pub async fn open_session_with_insecure(
        addr: SocketAddr,
        user_config: &UserConfig,
        server_key: Option<String>,
        mode: HostedAuthMode,
        allow_insecure: bool,
    ) -> Result<Self> {
        Ok(HostedSession::build(user_config, server_key, mode)?
            .with_allow_insecure(allow_insecure)
            .connect(addr)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    //! The connect/renewal-eligibility invariant lives here, in the one seam
    //! every hosted entry point opens its session through. This source-presence
    //! guard replaces the per-call-site checks: an eligible stored authority
    //! credential must be considered immediately after connect, while explicit
    //! and attenuated tokens carry no renewal binding.

    use biscuit_auth::{Biscuit, KeyPair};
    use crypto::{Ed25519Signer, Signer};

    use super::{validated_authenticated_principal, validated_stored_proof_key};
    use crate::credentials;

    fn proof_bound_credential(signer: &Ed25519Signer) -> credentials::ServerCredential {
        let token = Biscuit::builder()
            .fact(r#"user("alice")"#)
            .expect("user fact")
            .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
            .expect("proof key fact")
            .build(&KeyPair::new())
            .expect("mint credential")
            .to_base64()
            .expect("encode credential");
        credentials::ServerCredential {
            token,
            subject: "alice".to_string(),
            device_id: Some("device-1".to_string()),
            credential_id: None,
            private_key_pem: Some(signer.to_pem().expect("proof PEM")),
            expires_at: None,
        }
    }

    #[test]
    fn stored_session_rejects_missing_or_mismatched_proof_keys_before_connect() {
        let signer = Ed25519Signer::generate().expect("proof signer");
        let mut missing = proof_bound_credential(&signer);
        missing.private_key_pem = None;
        let error = validated_stored_proof_key(&missing, "grpc.example")
            .expect_err("a missing proof key must fail before connect");
        assert!(error.to_string().contains("has no device proof key"));

        let wrong_signer = Ed25519Signer::generate().expect("wrong proof signer");
        let mut mismatched = proof_bound_credential(&signer);
        mismatched.private_key_pem = Some(wrong_signer.to_pem().expect("wrong PEM"));
        let error = validated_stored_proof_key(&mismatched, "grpc.example")
            .expect_err("a mismatched proof key must fail before connect");
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn stored_session_accepts_the_proof_key_bound_into_its_biscuit() {
        let signer = Ed25519Signer::generate().expect("proof signer");
        let credential = proof_bound_credential(&signer);
        let pem =
            validated_stored_proof_key(&credential, "grpc.example").expect("matching proof key");
        assert_eq!(pem, credential.private_key_pem.unwrap());
    }

    #[test]
    fn stored_session_rejects_a_subject_that_disagrees_with_its_biscuit() {
        let signer = Ed25519Signer::generate().expect("proof signer");
        let mut credential = proof_bound_credential(&signer);
        credential.subject = "mallory".to_string();

        let error = validated_authenticated_principal(&credential)
            .expect_err("stored metadata cannot replace the authenticated token subject");

        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn session_connect_checks_eligible_renewal_after_connect() {
        let source = include_str!("session.rs");
        let connect_idx = source
            .find("HostedGrpcClient::connect(addr, &self.config)")
            .expect("session.rs must connect with the resolved addr");
        let after_connect = &source[connect_idx..];
        let rotate_offset = after_connect
            .find("auto_rotate_if_needed")
            .expect("auto_rotate_if_needed must appear in session.rs");
        assert!(
            rotate_offset < 400,
            "auto_rotate_if_needed must follow HostedGrpcClient::connect within the \
             same async block (found {rotate_offset} chars later)",
        );
    }

    #[test]
    fn explicit_derived_token_cannot_rotate_from_a_nearly_expired_stored_parent() {
        use biscuit_auth::{Biscuit, KeyPair};
        use crypto::{Ed25519Signer, Signer};

        use super::{HostedAuthMode, HostedSession};
        use crate::credentials;

        let _guard = credentials::lock_test_env();
        let home = tempfile::TempDir::new().expect("temp Heddle home");
        let previous_home = std::env::var_os("HEDDLE_HOME");
        let previous_remote_token = std::env::var_os("HEDDLE_REMOTE_TOKEN");
        unsafe {
            std::env::set_var("HEDDLE_HOME", home.path());
            std::env::remove_var("HEDDLE_REMOTE_TOKEN");
        }

        let result = std::panic::catch_unwind(|| {
            let parent_signer = Ed25519Signer::generate().expect("parent proof key");
            let expires_at = chrono::Utc::now() + chrono::Duration::minutes(5);
            let parent_token = Biscuit::builder()
                .fact(r#"user("alice")"#)
                .expect("user fact")
                .fact(r#"credential_id("cred-parent")"#)
                .expect("credential fact")
                .fact(
                    format!(
                        "device_pop_key(\"{}\")",
                        hex::encode(parent_signer.public_key())
                    )
                    .as_str(),
                )
                .expect("proof key fact")
                .fact(format!("expires_at({})", expires_at.to_rfc3339()).as_str())
                .expect("expiry fact")
                .build(&KeyPair::new())
                .expect("mint parent")
                .to_base64()
                .expect("encode parent");
            let server = "127.0.0.1:8421";
            let stored_parent = credentials::ServerCredential {
                token: parent_token.clone(),
                subject: "alice".to_string(),
                device_id: Some("device-parent".to_string()),
                credential_id: Some("cred-parent".to_string()),
                private_key_pem: Some(parent_signer.to_pem().expect("parent PEM")),
                expires_at: Some(expires_at.to_rfc3339()),
            };
            assert!(
                credentials::token_needs_rotation(&stored_parent),
                "the stored parent fixture must exercise the renewal window"
            );
            credentials::store_server_credential(server, stored_parent)
                .expect("store nearly expired parent");

            let child_signer = Ed25519Signer::generate().expect("child proof key");
            let child_token = crate::device_flow::attenuate_for_agent(
                &parent_token,
                crate::device_flow::AgentAttenuation {
                    agent_id: "explicit-child".to_string(),
                    expires_at: chrono::Utc::now() + chrono::Duration::minutes(3),
                    allowed_operations: Some(vec!["Push".to_string()]),
                    allowed_resources: None,
                    declared_scopes: Vec::new(),
                },
                &parent_signer,
                child_signer.public_key(),
            )
            .expect("derive explicit child");

            let stored_session = HostedSession::build(
                &cli_shared::UserConfig::default(),
                Some(server.to_string()),
                HostedAuthMode::CredentialFallback,
            )
            .expect("build stored-parent session");
            assert_eq!(
                stored_session
                    .config
                    .token
                    .as_ref()
                    .map(|token| token.id.as_str()),
                Some(parent_token.as_str())
            );
            assert!(
                stored_session.renewable_authority_credential.is_some(),
                "the exact stored authority token is renewable"
            );

            unsafe { std::env::set_var("HEDDLE_REMOTE_TOKEN", &child_token) };
            let explicit_child_session = HostedSession::build(
                &cli_shared::UserConfig::default(),
                Some(server.to_string()),
                HostedAuthMode::CredentialFallback,
            )
            .expect("build explicit-child session");
            assert_eq!(
                explicit_child_session
                    .config
                    .token
                    .as_ref()
                    .map(|token| token.id.as_str()),
                Some(child_token.as_str()),
                "the explicit child remains the active bearer"
            );
            assert!(
                explicit_child_session
                    .renewable_authority_credential
                    .is_none(),
                "an explicit derived bearer must not borrow the stored parent's renewal identity"
            );
        });

        unsafe {
            match previous_home {
                Some(path) => std::env::set_var("HEDDLE_HOME", path),
                None => std::env::remove_var("HEDDLE_HOME"),
            }
            match previous_remote_token {
                Some(token) => std::env::set_var("HEDDLE_REMOTE_TOKEN", token),
                None => std::env::remove_var("HEDDLE_REMOTE_TOKEN"),
            }
        }
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[tokio::test]
    async fn token_only_derived_child_does_not_fall_back_to_the_parent_device_key() {
        use biscuit_auth::{Biscuit, KeyPair};
        use crypto::{Ed25519Signer, Signer};
        use grpc::heddle::api::v1alpha1::{
            collaboration_service_client::CollaborationServiceClient,
            state_review_service_client::StateReviewServiceClient,
            identity_service_client::IdentityServiceClient,
            registry_service_client::RegistryServiceClient,
            repo_sync_service_client::RepoSyncServiceClient,
            repository_service_client::RepositoryServiceClient,
            workflow_service_client::WorkflowServiceClient,
        };
        use tonic::{Request, metadata::MetadataValue, transport::Endpoint};

        use super::{HostedAuthMode, HostedSession};
        use crate::{auth_cmd, credentials, grpc_hosted::HostedGrpcClient};

        let _guard = credentials::lock_test_env();
        let home = tempfile::TempDir::new().expect("temp Heddle home");
        let previous_home = std::env::var_os("HEDDLE_HOME");
        let previous_remote_token = std::env::var_os("HEDDLE_REMOTE_TOKEN");
        unsafe { std::env::set_var("HEDDLE_HOME", home.path()) };

        let result = std::panic::catch_unwind(|| {
            let signer = Ed25519Signer::generate().expect("device key");
            let private_key_pem = signer.to_pem().expect("device PEM");
            let key_path = home.path().join("bootstrap-key.pem");
            std::fs::write(&key_path, &private_key_pem).expect("write bootstrap key");

            let subject = "headless-agent";
            let credential_id = "cred-headless";
            let expires_at = chrono::Utc::now() + chrono::Duration::days(30);
            let user_fact = format!("user(\"{subject}\")");
            let credential_fact = format!("credential_id(\"{credential_id}\")");
            let expiry_fact = format!("expires_at({})", expires_at.to_rfc3339());
            let pop_fact = format!("device_pop_key(\"{}\")", hex::encode(signer.public_key()));
            let token = Biscuit::builder()
                .fact(user_fact.as_str())
                .expect("user fact")
                .fact(credential_fact.as_str())
                .expect("credential fact")
                .fact(expiry_fact.as_str())
                .expect("expiry fact")
                .fact(pop_fact.as_str())
                .expect("PoP key fact")
                .build(&KeyPair::new())
                .expect("mint fixture biscuit")
                .to_base64()
                .expect("encode fixture biscuit");
            let server = "127.0.0.1:8421";

            auth_cmd::install_headless_credential(server, &token, &key_path)
                .expect("headless credential install");

            let stored = credentials::get_server_credential(server)
                .expect("read credentials")
                .expect("stored credential");
            assert_eq!(stored.subject, subject);
            assert_eq!(stored.credential_id.as_deref(), Some(credential_id));
            assert_eq!(
                stored.private_key_pem.as_deref(),
                Some(private_key_pem.as_str())
            );
            assert!(stored.expires_at.is_some());
            let credential_file = std::fs::read_to_string(credentials::credentials_path())
                .expect("read installed credential file");
            assert!(credential_file.contains("private_key_pem ="));
            assert!(!credential_file.contains("\nprivate_key ="));

            let identity = repo::identity::load_device(&repo::identity::device_identity_path())
                .expect("load device identity")
                .expect("linked device identity");
            assert_eq!(identity.public_key, hex::encode(signer.public_key()));
            assert_eq!(identity.server, server);

            unsafe { std::env::set_var("HEDDLE_REMOTE_TOKEN", &token) };
            let root_session = HostedSession::build(
                &cli_shared::UserConfig::default(),
                Some(server.to_string()),
                HostedAuthMode::ConfigToken,
            )
            .expect("build root same-host session");
            assert_eq!(
                root_session.config.auth_proof_key_pem.as_deref(),
                Some(private_key_pem.as_str()),
                "a root token still resolves its matching same-host device key"
            );

            let child_signer = Ed25519Signer::generate().expect("child PoP key");
            let child_token = crate::device_flow::attenuate_for_agent(
                &token,
                crate::device_flow::AgentAttenuation {
                    agent_id: "agent-push".to_string(),
                    expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
                    allowed_operations: Some(vec!["Push".to_string()]),
                    allowed_resources: None,
                    declared_scopes: vec![("repo".to_string(), "acme/heddle".to_string())],
                },
                &signer,
                child_signer.public_key(),
            )
            .expect("derive child token");

            let child_private_key_pem = child_signer.to_pem().expect("child PEM");
            let child_key_path = home.path().join("child-key.pem");
            std::fs::write(&child_key_path, &child_private_key_pem)
                .expect("write child key for install");
            auth_cmd::install_headless_credential(server, &child_token, &child_key_path)
                .expect("install derived child credential");

            let stored_child = credentials::get_server_credential(server)
                .expect("read installed child credential")
                .expect("installed child credential");
            assert_eq!(
                stored_child.private_key_pem.as_deref(),
                Some(child_private_key_pem.as_str()),
                "the per-server child credential must retain its matching child key"
            );
            let preserved_identity =
                repo::identity::load_device(&repo::identity::device_identity_path())
                    .expect("reload shared device identity")
                    .expect("shared root identity remains installed");
            assert_eq!(
                preserved_identity.public_key,
                hex::encode(signer.public_key()),
                "installing a derived child must not replace the host-wide root identity"
            );
            assert_eq!(preserved_identity.private_key_pem, private_key_pem);

            unsafe { std::env::set_var("HEDDLE_REMOTE_TOKEN", &child_token) };

            let session = HostedSession::build(
                &cli_shared::UserConfig::default(),
                Some(server.to_string()),
                HostedAuthMode::ConfigToken,
            )
            .expect("build hosted session from token-only same-host handoff");
            let config = session.config;
            assert!(
                config.auth_proof_key_pem.is_none(),
                "a token-only child must not silently sign with its ancestor device key"
            );
            let channel = Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
            let client = HostedGrpcClient {
                inner: RepoSyncServiceClient::new(channel.clone())
                    .max_decoding_message_size(wire::MAX_PULL_DECODE_MESSAGE_SIZE),
                user: RegistryServiceClient::new(channel.clone()),
                auth: IdentityServiceClient::new(channel.clone()),
                content: RepositoryServiceClient::new(channel.clone()),
                workflow: WorkflowServiceClient::new(channel.clone()),
                collaboration: CollaborationServiceClient::new(channel.clone()),
                review: StateReviewServiceClient::new(channel),
                token_header: Some(
                    MetadataValue::try_from(format!(
                        "Bearer {}",
                        config.token.as_ref().expect("session token").id
                    ))
                    .expect("bearer metadata"),
                ),
                transport: crate::grpc_hosted::helpers::HostedTransportPolicy::from_client_config(
                    &config,
                ),
                auth_proof_key_pem: config.auth_proof_key_pem,
                authenticated_principal: config.authenticated_principal,
                server_key: config.server_key,
                on_human_signature: None,
            };
            let method = "/heddle.api.v1alpha1.RepoSyncService/Push";
            let mut request = Request::new(());
            client
                .apply_auth(&mut request, method)
                .expect("attach hosted auth");

            let metadata = request.metadata();
            assert!(metadata.get(crypto::pop::HDR_PROOF_TS).is_none());
            assert!(metadata.get(crypto::pop::HDR_PROOF_NONCE).is_none());
            assert!(metadata.get(crypto::pop::HDR_PROOF).is_none());
        });

        unsafe {
            match previous_home {
                Some(path) => std::env::set_var("HEDDLE_HOME", path),
                None => std::env::remove_var("HEDDLE_HOME"),
            }
            match previous_remote_token {
                Some(token) => std::env::set_var("HEDDLE_REMOTE_TOKEN", token),
                None => std::env::remove_var("HEDDLE_REMOTE_TOKEN"),
            }
        }
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
