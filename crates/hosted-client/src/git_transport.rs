//! Exchange an existing account/agent credential for ordinary Git's short-lived
//! HTTPS transport grant. No root minting, enrollment, or credential persistence.

use anyhow::{Context, Result, bail};
use api::heddle::api::v1alpha2 as wire;
use biscuit_verifier::git_transport::{GitChallenge, GitExchange, GitScope, credential_digest};
use chrono::{DateTime, Utc};
use crypto::Signer;
use prost::Message;

const MAX_RESPONSE: usize = 64 * 1024;

/// This secret is deliberately not `Debug` or serialized automatically. Put it
/// in Git's transient credential input or Authorization header, never its URL.
pub struct GitTransportGrant {
    token: String,
    pub expires_at: DateTime<Utc>,
}
impl GitTransportGrant {
    pub fn token(&self) -> &str {
        &self.token
    }
}

/// HTTPS only, with redirects and ambient proxies disabled so neither can copy
/// the Biscuit or proof to another service. The authority origin is explicit.
pub struct GitTransportClient {
    client: reqwest::Client,
    origin: reqwest::Url,
}
impl GitTransportClient {
    pub fn new(origin: &str) -> Result<Self> {
        let origin = reqwest::Url::parse(origin).context("Git authority URL")?;
        if origin.scheme() != "https"
            || origin.host_str().is_none()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !origin.username().is_empty()
            || origin.password().is_some()
        {
            bail!("Git authority must be an HTTPS origin without credentials, query or path");
        }
        // Match the hosted crate's existing convention, including standalone
        // consumers that have not initialized rustls through a native client.
        // An explicitly installed provider is retained if one already exists.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self { client, origin })
    }

    /// The supplied signer must own the credential's verified effective leaf
    /// key. The server checks possession and current account authority again.
    pub async fn exchange(
        &self,
        scope: &GitScope,
        biscuit: &str,
        signer: &impl Signer,
    ) -> Result<GitTransportGrant> {
        scope.validate()?;
        if signer.public_key().len() != 32 {
            bail!("Git exchange requires an Ed25519 signer");
        }
        let now = Utc::now();
        // Leave one minute for clock/network skew within the server's 5m cap.
        let expiry = now.timestamp() + 240;
        let scope_wire = scope_to_wire(scope);
        let digest = credential_digest(biscuit, None)?;
        let request = wire::GitTransportChallengeRequest {
            scope: Some(scope_wire.clone()),
            credential_digest: digest.to_vec(),
            session_expires_at_seconds: expiry,
        };
        let response: wire::GitTransportChallengeResponse =
            self.post("/git/auth/challenge", &request).await?;
        let challenge = response.challenge.context("missing Git challenge")?;
        let exchange = GitExchange {
            scope: scope.clone(),
            credential_digest: digest,
            challenge: GitChallenge {
                nonce: challenge
                    .nonce
                    .as_slice()
                    .try_into()
                    .context("invalid Git nonce")?,
                issued_at_seconds: challenge.issued_at_seconds,
                expires_at_seconds: challenge.expires_at_seconds,
            },
            session_expires_at_seconds: expiry,
        };
        exchange.validate_at(Utc::now())?;
        let signature = signer.sign(&exchange.signing_bytes()?)?;
        let request = wire::GitTransportExchangeRequest {
            scope: Some(scope_wire),
            biscuit: biscuit.to_owned(),
            grant_envelope: None,
            challenge: Some(challenge),
            session_expires_at_seconds: expiry,
            proof_signature: signature,
        };
        let response: wire::GitTransportExchangeResponse =
            self.post("/git/auth/exchange", &request).await?;
        let expires_at = DateTime::from_timestamp(response.expires_at_seconds, 0)
            .context("invalid Git session expiry")?;
        let secret = response
            .transport_token
            .strip_prefix("ggit1_")
            .context("invalid Git session credential")?;
        if secret.len() != 64
            || !secret
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || expires_at <= Utc::now()
            || expires_at.timestamp() > expiry
        {
            bail!("invalid Git session response");
        }
        Ok(GitTransportGrant {
            token: response.transport_token,
            expires_at,
        })
    }

    /// Recheck the exact session scope before a Git effect. This answers only
    /// session/account authority; the host must separately resolve the selected
    /// source-history closure's visibility, redaction and retained admission.
    pub async fn authorize(
        &self,
        scope: &GitScope,
        transport_token: &str,
    ) -> Result<wire::GitTransportAuthorizeResponse> {
        scope.validate()?;
        let response: wire::GitTransportAuthorizeResponse = self
            .post(
                "/git/auth/authorize",
                &wire::GitTransportAuthorizeRequest {
                    transport_token: transport_token.to_owned(),
                    expected_scope: Some(scope_to_wire(scope)),
                },
            )
            .await?;
        let actor = response
            .actor
            .as_ref()
            .context("missing current Git actor")?;
        let actor = uuid::Uuid::parse_str(&actor.id).context("invalid current Git actor")?;
        if actor.is_nil()
            || response.authorization_epoch <= 0
            || response.expires_at_seconds <= Utc::now().timestamp()
            || response.credential_digest.len() != 32
        {
            bail!("invalid current Git session authority");
        }
        Ok(response)
    }

    async fn post<Q: Message, R: Message + Default>(&self, path: &str, request: &Q) -> Result<R> {
        let bytes = request.encode_to_vec();
        if bytes.len() > MAX_RESPONSE {
            bail!("Git exchange request exceeds limit");
        }
        let mut response = self
            .client
            .post(self.origin.join(path)?)
            .header("content-type", "application/x-protobuf")
            .body(bytes)
            .send()
            .await
            .context("Git authority unavailable")?;
        if !response.status().is_success() {
            bail!("Git authority refused the exchange ({})", response.status());
        }
        if response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            != Some("application/x-protobuf")
            || response
                .content_length()
                .is_some_and(|length| length > MAX_RESPONSE as u64)
        {
            bail!("invalid Git authority response");
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                bail!("Git authority response exceeds limit");
            }
            body.extend_from_slice(&chunk);
        }
        R::decode(body.as_slice()).context("invalid Git authority response")
    }
}
fn scope_to_wire(scope: &GitScope) -> wire::GitTransportScope {
    wire::GitTransportScope {
        service_audience: scope.service_audience.clone(),
        tenant_spool: Some(wire::SpoolRef {
            id: scope.tenant_spool_id.to_string(),
        }),
        spool: Some(wire::SpoolRef {
            id: scope.spool_id.to_string(),
        }),
        repository_path: scope.repository_path.clone(),
        thread: Some(wire::ThreadId {
            value: scope.thread_id.to_vec(),
        }),
        action: match scope.action {
            biscuit_verifier::git_transport::GitAction::Read => {
                wire::GitTransportAction::Read as i32
            }
            biscuit_verifier::git_transport::GitAction::Write => {
                wire::GitTransportAction::Write as i32
            }
        },
        disclosure_audience: scope.disclosure_audience.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exchange_never_sends_a_credential_to_http_or_an_ambiguous_url() {
        for url in [
            "http://authority.example/",
            "https://user:secret@authority.example/",
            "https://authority.example/path",
            "https://authority.example/?next=elsewhere",
            "https://authority.example/#other",
        ] {
            assert!(GitTransportClient::new(url).is_err(), "{url}");
        }
    }
}
