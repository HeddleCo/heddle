//! Hosted-session open seam.
//!
//! "Open a usable hosted session for a remote" used to be hand-assembled at
//! every hosted entry point (push, pull, fetch, clone, support, approval,
//! lazy hydration): resolve the auth token, fall back to the credential
//! store, attach the proof key, build the validated client config, connect,
//! then run mandatory credential rotation. This module owns that assembly so
//! the command modules choose only intent + remote and call one seam.

use std::net::SocketAddr;

use anyhow::Result;
use cli_shared::{ClientConfig, UserConfig};
use wire::{AuthToken, ProtocolError};

use crate::{credentials, grpc_hosted::HostedGrpcClient};

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
}

impl HostedSession {
    /// Resolve auth + build the validated client config for a hosted session.
    /// Owns credential-store fallback (per `mode`), server-key attachment, and
    /// proof-key attachment — the assembly the command modules used to
    /// hand-roll.
    pub fn build(
        user_config: &UserConfig,
        server_key: Option<String>,
        mode: HostedAuthMode,
    ) -> Result<Self> {
        let (token, credential_proof_key) = match mode {
            HostedAuthMode::ConfigToken => (user_config.remote_token()?, None),
            HostedAuthMode::CredentialFallback => {
                let mut token = user_config.remote_token()?;
                let mut credential_proof_key = None;
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
                        token = Some(AuthToken::new(cred.token, "credential-store"));
                        credential_proof_key = cred.private_key_pem;
                    }
                }
                (token, credential_proof_key)
            }
        };

        let mut config = user_config.heddle_client_config(token)?;
        if let Some(key) = server_key {
            config = config.with_server_key(key);
        }
        if let Some(pem) = credential_proof_key
            && config.auth_proof_key_pem.is_none()
        {
            config = config.with_auth_proof_key_pem(pem);
        }
        Ok(Self { config })
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

    /// Connect and run mandatory credential rotation.
    ///
    /// The rotation MUST run immediately after connect — every hosted entry
    /// point relies on a fresh token before its first RPC. This is the single
    /// place that pairs connect with rotation; see the source-presence guard
    /// in this module's tests.
    pub async fn connect(&self, addr: SocketAddr) -> Result<HostedGrpcClient, ProtocolError> {
        let mut client = HostedGrpcClient::connect(addr, &self.config).await?;
        client.auto_rotate_if_needed().await;
        Ok(client)
    }
}

impl HostedGrpcClient {
    /// Open a usable hosted session in one call: resolve auth, build the
    /// validated client config, connect, and run mandatory rotation. Callers
    /// that must prevalidate the config before irreversible work build a
    /// [`HostedSession`] first, then [`HostedSession::connect`].
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
    //! The connect/rotate invariant now lives here, in the one seam every
    //! hosted entry point opens its session through. This source-presence
    //! guard replaces the per-call-site rotation checks: rotation MUST run
    //! immediately after `HostedGrpcClient::connect`, or a process whose
    //! cached token has slipped past expiry hits an auth failure on its first
    //! RPC even though the rotation data is on disk.

    #[test]
    fn session_connect_rotates_credentials_after_connect() {
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

    #[tokio::test]
    async fn headless_install_supplies_a_verifiable_hosted_proof() {
        use base64::Engine as _;
        use biscuit_auth::{Biscuit, KeyPair};
        use crypto::{Ed25519Signer, Signer};
        use grpc::heddle::api::v1alpha1::{
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

            let session = HostedSession::build(
                &cli_shared::UserConfig::default(),
                Some(server.to_string()),
                HostedAuthMode::CredentialFallback,
            )
            .expect("build hosted session from installed credential");
            let config = session.config;
            let channel = Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
            let client = HostedGrpcClient {
                inner: RepoSyncServiceClient::new(channel.clone())
                    .max_decoding_message_size(wire::MAX_PULL_DECODE_MESSAGE_SIZE),
                user: RegistryServiceClient::new(channel.clone()),
                auth: IdentityServiceClient::new(channel.clone()),
                content: RepositoryServiceClient::new(channel.clone()),
                workflow: WorkflowServiceClient::new(channel),
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
                server_key: config.server_key,
                on_human_signature: None,
            };
            let method = "/heddle.api.v1alpha1.RepoSyncService/Push";
            let mut request = Request::new(());
            client
                .apply_auth(&mut request, method)
                .expect("attach hosted auth");

            let metadata = request.metadata();
            let proof_ts = metadata
                .get(crypto::pop::HDR_PROOF_TS)
                .and_then(|value| value.to_str().ok())
                .expect("proof timestamp");
            let nonce = metadata
                .get(crypto::pop::HDR_PROOF_NONCE)
                .and_then(|value| value.to_str().ok())
                .expect("proof nonce");
            let proof = metadata
                .get(crypto::pop::HDR_PROOF)
                .and_then(|value| value.to_str().ok())
                .expect("proof signature");
            let signature = base64::engine::general_purpose::STANDARD
                .decode(proof)
                .expect("decode proof signature");
            crypto::pop::verify_pop(
                signer.public_key(),
                &token,
                proof_ts,
                "POST",
                method,
                nonce,
                &signature,
            )
            .expect("proof verifies against installed device key");
        });

        unsafe {
            match previous_home {
                Some(path) => std::env::set_var("HEDDLE_HOME", path),
                None => std::env::remove_var("HEDDLE_HOME"),
            }
        }
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
