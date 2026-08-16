//! Validated native hosted-session assembly and signed descriptor bootstrap.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use cli_shared::{ClientConfig, UserConfig};
use wire::ProtocolError;

use super::{
    HostedClient, RenewableAuthorityCredential, credential::server_keys_match,
    resolve_hosted_credential, resolver::resolve_and_verify_endpoint_descriptor,
};

pub enum HostedAuthMode {
    Unauthenticated,
    ProofOnly {
        proof_key_pem: String,
        signing_identity: String,
    },
    CredentialFallback,
}

pub struct HostedSession {
    config: ClientConfig,
    renewable_authority_credential: Option<RenewableAuthorityCredential>,
}

impl HostedSession {
    pub fn build(
        user_config: &UserConfig,
        server_key: Option<String>,
        mode: HostedAuthMode,
    ) -> Result<Self> {
        let (
            token,
            mut credential_proof_key,
            renewable_authority_credential,
            resolved_credential_subject,
        ) = match mode {
            HostedAuthMode::Unauthenticated => (None, None, None, None),
            HostedAuthMode::ProofOnly {
                proof_key_pem,
                signing_identity,
            } => (None, Some(proof_key_pem), None, Some(signing_identity)),
            HostedAuthMode::CredentialFallback => {
                let resolved = resolve_hosted_credential(server_key.as_deref())?;
                (
                    resolved.token,
                    resolved.proof_key_pem,
                    resolved.renewable,
                    resolved.subject,
                )
            }
        };

        if credential_proof_key.is_none()
            && let Some(ref key) = server_key
            && let Some(token) = token.as_ref()
        {
            credential_proof_key = shared_device_proof_key(key, &token.id)?;
        }

        let mut config = user_config.hosted_runtime_config(token)?;
        if let Some(key) = server_key {
            config = config.with_server_key(key);
        }
        if let Some(pem) = credential_proof_key
            && config.auth_proof_key_pem.is_none()
        {
            config = config.with_auth_proof_key_pem(pem);
        }
        if config.auth_proof_key_pem.is_some() {
            if let Some(token) = config.token.as_ref() {
                let subject = crate::hosted_runtime::device_flow::authenticated_subject(&token.id)
                    .context("reading the hosted bearer token's authenticated principal")?;
                if resolved_credential_subject
                    .as_deref()
                    .is_some_and(|stored| stored != subject.as_str())
                {
                    anyhow::bail!(
                        "resolved credential subject does not match the bearer token's authenticated principal"
                    );
                }
                config = config.with_authenticated_principal(format!("principal:{subject}"));
            } else if let Some(identity) = resolved_credential_subject {
                config = config.with_authenticated_principal(identity);
            } else {
                anyhow::bail!("hosted request signing has no stable signing identity");
            }
        }
        Ok(Self {
            config,
            renewable_authority_credential,
        })
    }

    pub fn with_allow_insecure(mut self, allow: bool) -> Self {
        if allow {
            self.config.allow_insecure = true;
        }
        self
    }

    pub async fn discover_endpoint(
        &self,
        fallback_addr: SocketAddr,
    ) -> super::Result<super::VerifiedEndpointDescriptor> {
        let server = self
            .config
            .server_key
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| fallback_addr.to_string());
        resolve_and_verify_endpoint_descriptor(&server, &self.config).await
    }

    pub async fn connect(&self, fallback_addr: SocketAddr) -> Result<HostedClient, ProtocolError> {
        let descriptor = self
            .discover_endpoint(fallback_addr)
            .await
            .map_err(|error| ProtocolError::Remote(error.to_string()))?;
        let mut client = HostedClient::connect_with_config(&descriptor, &self.config)
            .await
            .map_err(|error| ProtocolError::Remote(error.to_string()))?;
        client
            .auto_rotate_if_needed(self.renewable_authority_credential.as_ref())
            .await;
        Ok(client)
    }
}

impl HostedClient {
    /// Connect to a hosted server through its signed HTTPS endpoint descriptor.
    ///
    /// This is the transport-neutral entry point for callers that already
    /// assembled a [`ClientConfig`] (for example an operator CLI). Descriptor
    /// verification and Iroh address selection remain inside the hosted-call
    /// module instead of being repeated by each caller.
    pub async fn connect_server(server: &str, config: &ClientConfig) -> Result<Self> {
        let descriptor = resolve_and_verify_endpoint_descriptor(server, config).await?;
        Ok(Self::connect_with_config(&descriptor, config).await?)
    }

    pub async fn open_session(
        addr: SocketAddr,
        user_config: &UserConfig,
        server_key: Option<String>,
        mode: HostedAuthMode,
    ) -> Result<Self> {
        Self::open_session_with_insecure(addr, user_config, server_key, mode, false).await
    }

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

fn shared_device_proof_key(server_key: &str, token: &str) -> Result<Option<String>> {
    let identity = repo::identity::load_device(&repo::identity::device_identity_path())
        .context("loading this host's shared device identity")?;
    let Some(identity) = identity else {
        return Ok(None);
    };
    if !server_keys_match(&identity.server, server_key)
        || !crate::hosted_runtime::device_flow::effective_pop_public_key_hex(token)
            .is_ok_and(|key| key.eq_ignore_ascii_case(&identity.public_key))
    {
        return Ok(None);
    }
    Ok(Some(identity.private_key_pem))
}
