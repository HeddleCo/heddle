//! Validated native hosted-session assembly and signed descriptor bootstrap.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use cli_shared::{ClientConfig, UserConfig};
use crypto::{Ed25519Signer, Signer};
use wire::{AuthToken, ProtocolError};

use super::{
    DescriptorKeyring, HostedClient, RenewableAuthorityCredential, credential::server_keys_match,
    fetch_endpoint_descriptor, resolve_hosted_credential,
};
use crate::credentials;

pub enum HostedAuthMode {
    Unauthenticated,
    CredentialFallback,
}

pub struct HostedSession {
    config: ClientConfig,
    renewable_authority_credential: Option<RenewableAuthorityCredential>,
}

impl HostedSession {
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
        let config = user_config
            .heddle_client_config(Some(token))?
            .with_server_key(server_key.to_string())
            .with_auth_proof_key_pem(proof_key)
            .with_authenticated_principal(authenticated_principal);
        Ok(Self {
            config,
            renewable_authority_credential,
        })
    }

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
            HostedAuthMode::Unauthenticated => (None, None, None, None),
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

    pub fn with_allow_insecure(mut self, allow: bool) -> Self {
        if allow {
            self.config.allow_insecure = true;
        }
        self
    }

    pub async fn connect(&self, fallback_addr: SocketAddr) -> Result<HostedClient, ProtocolError> {
        let key_id = self.config.descriptor_key_id.as_deref().ok_or_else(|| {
            ProtocolError::InvalidState(
                "native hosted transport requires a trusted descriptor key id".to_string(),
            )
        })?;
        let public_key = self.config.descriptor_public_key.ok_or_else(|| {
            ProtocolError::InvalidState(
                "native hosted transport requires a trusted descriptor public key".to_string(),
            )
        })?;
        let mut keys = DescriptorKeyring::default();
        keys.insert(key_id, public_key, i64::MIN, i64::MAX)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let server = self
            .config
            .server_key
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| fallback_addr.to_string());
        let descriptor = fetch_endpoint_descriptor(&descriptor_url(&server)?, &keys)
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
        let key_id = config.descriptor_key_id.as_deref().ok_or_else(|| {
            anyhow::anyhow!("native hosted transport requires a trusted descriptor key id")
        })?;
        let public_key = config.descriptor_public_key.ok_or_else(|| {
            anyhow::anyhow!("native hosted transport requires a trusted descriptor public key")
        })?;
        let mut keys = DescriptorKeyring::default();
        keys.insert(key_id, public_key, i64::MIN, i64::MAX)?;
        let descriptor = fetch_endpoint_descriptor(&descriptor_url(server)?, &keys).await?;
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

fn descriptor_url(server: &str) -> Result<String, ProtocolError> {
    let authority = server
        .strip_prefix("https://")
        .or_else(|| server.strip_prefix("heddle://"))
        .unwrap_or(server)
        .trim_end_matches('/');
    if server.starts_with("http://") || authority.is_empty() || authority.contains('/') {
        return Err(ProtocolError::InvalidState(
            "native hosted bootstrap requires an HTTPS server authority".to_string(),
        ));
    }
    Ok(format!(
        "https://{authority}/.well-known/heddle/iroh-endpoint"
    ))
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
        || !crate::device_flow::effective_pop_public_key_hex(token)
            .is_ok_and(|key| key.eq_ignore_ascii_case(&identity.public_key))
    {
        return Ok(None);
    }
    Ok(Some(identity.private_key_pem))
}

#[cfg(test)]
mod tests {
    use super::descriptor_url;

    #[test]
    fn descriptor_bootstrap_is_https_and_well_known() {
        assert_eq!(
            descriptor_url("heddle://weft.example:8421").unwrap(),
            "https://weft.example:8421/.well-known/heddle/iroh-endpoint"
        );
        assert!(descriptor_url("http://weft.example:8421").is_err());
    }

    #[tokio::test]
    async fn connect_server_requires_a_descriptor_trust_root_before_network_io() {
        let error = crate::hosted::HostedClient::connect_server(
            "weft.example:8421",
            &cli_shared::ClientConfig::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("trusted descriptor key id"));
    }
}
