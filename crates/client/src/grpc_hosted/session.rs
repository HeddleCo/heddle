//! Hosted-session open seam.
//!
//! "Open a usable hosted session for a remote" used to be hand-assembled at
//! every hosted entry point (push, pull, fetch, clone, support, approval,
//! lazy hydration): resolve the auth token, fall back to the credential
//! store, attach the proof key, build the validated client config, connect,
//! then rotate only an eligible stored authority credential. This module owns
//! that assembly so the command modules choose only intent + remote and call
//! one seam.
//!
//! # Single credential resolution order
//!
//! [`resolve_hosted_credential`] is the ONE precedence every hosted entry
//! point follows:
//!
//! 1. `HEDDLE_CREDENTIAL=<path>` — a path to a `.hcred`. When set it is
//!    AUTHORITATIVE: an unreadable / expired / verify-failed / server-mismatch
//!    file is a hard error, never a silent fall-through to the keystore (a
//!    silent fallback would let an agent push as the human). Loaded through the
//!    verifying [`crate::credential_file::load_credential_file`] chokepoint.
//! 2. The per-server keystore entry (`resolve_credential_for_server`).
//! 3. Unauthenticated.

use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use cli_shared::{ClientConfig, UserConfig};
use crypto::{Ed25519Signer, Signer};
use tonic::transport::Channel;
use wire::{AuthToken, ProtocolError};

use crate::{
    credentials,
    grpc_hosted::{HostedGrpcClient, RenewableAuthorityCredential},
};

/// Environment variable naming a `.hcred` path the runtime authenticates with.
const HEDDLE_CREDENTIAL_ENV: &str = "HEDDLE_CREDENTIAL";

/// Where a resolved hosted credential originated. Reported by `auth status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// The `.hcred` at `HEDDLE_CREDENTIAL=<path>`.
    Env(PathBuf),
    /// The per-server entry in the keystore (`credentials.toml`).
    Keystore,
    /// No credential resolved for this server.
    Unauthenticated,
}

impl CredentialSource {
    /// Stable label for the `auth status` `source` field.
    pub fn label(&self) -> String {
        match self {
            CredentialSource::Env(path) => format!("env:{}", path.display()),
            CredentialSource::Keystore => "keystore".to_string(),
            CredentialSource::Unauthenticated => "none".to_string(),
        }
    }
}

/// A credential resolved by the single hosted-auth precedence. Fully describes
/// the selected bearer so callers never re-read env/keystore themselves.
pub struct ResolvedHostedCredential {
    /// Bearer token, absent when unauthenticated.
    pub token: Option<AuthToken>,
    /// Device proof key PEM bound to `token`, when the source carries one.
    pub proof_key_pem: Option<String>,
    /// Renewal binding — populated ONLY for a keystore authority credential.
    /// An env `.hcred` is never renewed (we cannot rewrite the pointed-at file
    /// and must not touch the keystore on its behalf). Crate-internal: the
    /// renewal type is module-private and only [`HostedSession::build`]
    /// consumes it.
    pub(crate) renewable: Option<RenewableAuthorityCredential>,
    /// Authenticated subject.
    pub subject: Option<String>,
    /// Authority credential id, when present (rotation anchor).
    pub credential_id: Option<String>,
    /// RFC 3339 expiry, when the credential carries one.
    pub expires_at: Option<String>,
    /// Which mechanism produced this credential.
    pub source: CredentialSource,
}

impl std::fmt::Debug for ResolvedHostedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the secret bearer + proof key so a resolved credential can
        // never leak them into logs, tracing, or a test panic message.
        f.debug_struct("ResolvedHostedCredential")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("proof_key_pem", &self.proof_key_pem.as_ref().map(|_| "<redacted>"))
            .field("renewable", &self.renewable.is_some())
            .field("subject", &self.subject)
            .field("credential_id", &self.credential_id)
            .field("expires_at", &self.expires_at)
            .field("source", &self.source)
            .finish()
    }
}

/// Read + validate `HEDDLE_CREDENTIAL`. Returns the `.hcred` path when the
/// variable is set to a non-empty value. Guards against a caller passing
/// credential *contents* instead of a path — a path never starts with `{`
/// (the `.hcred` JSON object) and never contains a newline.
fn heddle_credential_env_path() -> Result<Option<PathBuf>> {
    match std::env::var(HEDDLE_CREDENTIAL_ENV) {
        Ok(value) => {
            if value.is_empty() {
                // Fail closed rather than fall through to the keystore: a set-but-empty
                // value is almost always a failed shell expansion (e.g.
                // `export HEDDLE_CREDENTIAL=$UNSET`), and silently authenticating as
                // the stored (human) identity is exactly the confusion this resolver
                // exists to prevent.
                anyhow::bail!(
                    "HEDDLE_CREDENTIAL is set but empty; unset it to use the stored \
                     credential, or point it at a .hcred file"
                );
            }
            if value.starts_with('{') || value.contains('\n') {
                anyhow::bail!(
                    "HEDDLE_CREDENTIAL takes a file path, not credential contents"
                );
            }
            Ok(Some(PathBuf::from(value)))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err @ std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("HEDDLE_CREDENTIAL is not valid UTF-8: {err}")
        }
    }
}

/// Resolve the hosted credential for `server_key` following the single
/// precedence order documented on this module.
///
/// `HEDDLE_CREDENTIAL`, when set, is authoritative: any failure to load, verify,
/// or match it to `server_key` is a hard error rather than a silent fall-through
/// to the keystore.
pub fn resolve_hosted_credential(server_key: Option<&str>) -> Result<ResolvedHostedCredential> {
    if let Some(path) = heddle_credential_env_path()? {
        let verified = crate::credential_file::load_credential_file(&path)
            .with_context(|| format!("loading HEDDLE_CREDENTIAL {}", path.display()))?;
        if let Some(target) = server_key
            && !server_keys_match(&verified.server, target)
        {
            anyhow::bail!(
                "HEDDLE_CREDENTIAL {} authenticates server {:?}, but this operation targets {:?}; \
                 point HEDDLE_CREDENTIAL at a credential minted for {}",
                path.display(),
                verified.server,
                target,
                target,
            );
        }
        return Ok(ResolvedHostedCredential {
            token: Some(AuthToken::new(verified.token, "hcred-env")),
            proof_key_pem: Some(verified.proof_key_pem),
            renewable: None,
            subject: Some(verified.subject),
            credential_id: verified.credential_id,
            expires_at: verified.expires_at,
            source: CredentialSource::Env(path),
        });
    }

    if let Some(key) = server_key {
        // Propagate a malformed credentials.toml instead of swallowing it: a
        // parse error here used to fall through to an unauthenticated request,
        // which the server rejects with an opaque "missing authorization
        // metadata" — hiding the real cause. A *missing* file returns Ok(None)
        // and falls through to unauthenticated cleanly.
        if let Some(cred) = credentials::resolve_credential_for_server(key)? {
            let renewable = RenewableAuthorityCredential::from_stored(&cred);
            return Ok(ResolvedHostedCredential {
                token: Some(AuthToken::new(cred.token, "credential-store")),
                proof_key_pem: cred.private_key_pem,
                renewable,
                subject: Some(cred.subject),
                credential_id: cred.credential_id,
                expires_at: cred.expires_at,
                source: CredentialSource::Keystore,
            });
        }
    }

    Ok(ResolvedHostedCredential {
        token: None,
        proof_key_pem: None,
        renewable: None,
        subject: None,
        credential_id: None,
        expires_at: None,
        source: CredentialSource::Unauthenticated,
    })
}

/// Resolve the locally active bearer token, if any, using the default server
/// for keystore lookup. For read/display paths that need the active token
/// without a specific target remote. Still honors `HEDDLE_CREDENTIAL` first.
pub fn resolve_active_bearer() -> Result<Option<AuthToken>> {
    let server = credentials::default_server()?;
    Ok(resolve_hosted_credential(server.as_deref())?.token)
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
    /// Runs the single [`resolve_hosted_credential`] precedence
    /// (`HEDDLE_CREDENTIAL` → keystore → unauthenticated), server-key
    /// attachment, and proof-key attachment from either the resolved credential
    /// or the matching shared same-host device identity — the assembly the
    /// command modules used to hand-roll.
    pub fn build(user_config: &UserConfig, server_key: Option<String>) -> Result<Self> {
        let ResolvedHostedCredential {
            token,
            proof_key_pem: mut credential_proof_key,
            renewable: renewable_authority_credential,
            subject: resolved_credential_subject,
            ..
        } = resolve_hosted_credential(server_key.as_deref())?;

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
            if resolved_credential_subject
                .as_deref()
                .is_some_and(|resolved| resolved != subject.as_str())
            {
                anyhow::bail!(
                    "resolved credential subject does not match the bearer token's authenticated principal"
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
    ) -> Result<Self> {
        Self::open_session_with_insecure(addr, user_config, server_key, false).await
    }

    /// Like [`open_session`], but allows cleartext to non-loopback when
    /// `allow_insecure` is true (CLI `--insecure` / remote `insecure = true`).
    pub async fn open_session_with_insecure(
        addr: SocketAddr,
        user_config: &UserConfig,
        server_key: Option<String>,
        allow_insecure: bool,
    ) -> Result<Self> {
        Ok(HostedSession::build(user_config, server_key)?
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

    use super::{HostedSession, validated_authenticated_principal, validated_stored_proof_key};
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

    /// Mint a device-authority token whose effective PoP key is `signer`.
    fn mint_authority_token(subject: &str, signer: &Ed25519Signer) -> String {
        biscuit_auth::Biscuit::builder()
            .fact(format!("user(\"{subject}\")").as_str())
            .expect("user fact")
            .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
            .expect("proof key fact")
            .build(&biscuit_auth::KeyPair::new())
            .expect("mint token")
            .to_base64()
            .expect("encode token")
    }

    /// Write a verifiable `.hcred` at `path` for `server`, minting a fresh
    /// device token bound to a fresh proof key.
    fn write_sample_hcred(path: &std::path::Path, server: &str, subject: &str) {
        let signer = Ed25519Signer::generate().expect("proof key");
        let token = mint_authority_token(subject, &signer);
        crate::credential_file::write_credential_file(
            path,
            &crate::credential_file::VerifiedCredential {
                server: server.to_string(),
                kind: crate::credential_file::CredentialKind::Device,
                subject: subject.to_string(),
                token,
                proof_key_pem: signer.to_pem().expect("proof PEM"),
                expires_at: None,
                credential_id: None,
                provenance: None,
            },
        )
        .expect("write sample .hcred");
    }

    /// Run `f` with `HEDDLE_HOME` at a fresh temp dir and `HEDDLE_CREDENTIAL`
    /// cleared, restoring both afterward. Serialized via the credentials lock.
    fn with_isolated_env<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = crate::credentials::lock_test_env();
        let home = tempfile::TempDir::new().expect("temp Heddle home");
        let previous_home = std::env::var_os("HEDDLE_HOME");
        let previous_credential = std::env::var_os("HEDDLE_CREDENTIAL");
        unsafe {
            std::env::set_var("HEDDLE_HOME", home.path());
            std::env::remove_var("HEDDLE_CREDENTIAL");
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(home.path())));
        unsafe {
            match previous_home {
                Some(path) => std::env::set_var("HEDDLE_HOME", path),
                None => std::env::remove_var("HEDDLE_HOME"),
            }
            match previous_credential {
                Some(path) => std::env::set_var("HEDDLE_CREDENTIAL", path),
                None => std::env::remove_var("HEDDLE_CREDENTIAL"),
            }
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    #[test]
    fn env_credential_resolves_and_reports_env_source() {
        use super::{CredentialSource, resolve_hosted_credential};
        with_isolated_env(|home| {
            let path = home.join("agent.hcred");
            write_sample_hcred(&path, "grpc.heddle.test", "alice");
            unsafe { std::env::set_var("HEDDLE_CREDENTIAL", &path) };

            let resolved =
                resolve_hosted_credential(Some("grpc.heddle.test")).expect("resolve env credential");
            assert!(resolved.token.is_some(), "env credential is authoritative");
            assert!(resolved.proof_key_pem.is_some(), "env .hcred carries its key");
            assert!(
                resolved.renewable.is_none(),
                "an env credential is never renewed"
            );
            assert_eq!(resolved.subject.as_deref(), Some("alice"));
            assert_eq!(resolved.source, CredentialSource::Env(path));
        });
    }

    #[test]
    fn env_credential_inline_contents_are_rejected() {
        use super::resolve_hosted_credential;
        with_isolated_env(|_home| {
            unsafe {
                std::env::set_var("HEDDLE_CREDENTIAL", "{\"format\":\"heddle-credential\"}");
            }
            let error = resolve_hosted_credential(Some("grpc.heddle.test"))
                .expect_err("inline contents must be rejected");
            assert!(
                error.to_string().contains("takes a file path"),
                "unexpected error: {error}"
            );
        });
    }

    #[test]
    fn env_credential_server_mismatch_is_a_hard_error_with_no_keystore_fallback() {
        use super::resolve_hosted_credential;
        with_isolated_env(|home| {
            // A perfectly good keystore credential exists for the target
            // server. It must NOT be used to paper over a mismatched
            // HEDDLE_CREDENTIAL — silent fallback would let an agent push as
            // the human.
            credentials::store_server_credential(
                "grpc.target.test",
                credentials::ServerCredential {
                    token: "keystore-token".to_string(),
                    subject: "human".to_string(),
                    device_id: None,
                    credential_id: None,
                    private_key_pem: None,
                    expires_at: None,
                },
            )
            .expect("seed keystore");

            let path = home.join("other.hcred");
            write_sample_hcred(&path, "grpc.other.test", "agent");
            unsafe { std::env::set_var("HEDDLE_CREDENTIAL", &path) };

            let error = resolve_hosted_credential(Some("grpc.target.test"))
                .expect_err("server mismatch must be a hard error");
            let message = error.to_string();
            assert!(message.contains("grpc.other.test"), "message: {message}");
            assert!(message.contains("grpc.target.test"), "message: {message}");
        });
    }

    #[test]
    fn env_credential_unreadable_is_a_hard_error_with_no_keystore_fallback() {
        use super::resolve_hosted_credential;
        with_isolated_env(|home| {
            credentials::store_server_credential(
                "grpc.target.test",
                credentials::ServerCredential {
                    token: "keystore-token".to_string(),
                    subject: "human".to_string(),
                    device_id: None,
                    credential_id: None,
                    private_key_pem: None,
                    expires_at: None,
                },
            )
            .expect("seed keystore");

            let missing = home.join("does-not-exist.hcred");
            unsafe { std::env::set_var("HEDDLE_CREDENTIAL", &missing) };

            resolve_hosted_credential(Some("grpc.target.test"))
                .expect_err("an unreadable HEDDLE_CREDENTIAL must never fall back to the keystore");
        });
    }

    #[test]
    fn env_credential_is_authoritative_and_not_renewable_over_a_stored_parent() {
        use crypto::{Ed25519Signer, Signer};

        with_isolated_env(|home| {
            let parent_signer = Ed25519Signer::generate().expect("parent proof key");
            let expires_at = chrono::Utc::now() + chrono::Duration::minutes(5);
            let parent_token = biscuit_auth::Biscuit::builder()
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
                .build(&biscuit_auth::KeyPair::new())
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

            // Without HEDDLE_CREDENTIAL, the keystore parent is the active,
            // renewable authority.
            let stored_session =
                HostedSession::build(&cli_shared::UserConfig::default(), Some(server.to_string()))
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

            // The same child, handed to the runtime as a HEDDLE_CREDENTIAL
            // .hcred, overrides the keystore and is never renewed.
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
            let child_hcred = home.join("child.hcred");
            crate::credential_file::write_credential_file(
                &child_hcred,
                &crate::credential_file::VerifiedCredential {
                    server: server.to_string(),
                    kind: crate::credential_file::CredentialKind::Agent,
                    subject: "alice".to_string(),
                    token: child_token.clone(),
                    proof_key_pem: child_signer.to_pem().expect("child PEM"),
                    expires_at: Some(
                        (chrono::Utc::now() + chrono::Duration::minutes(3)).to_rfc3339(),
                    ),
                    credential_id: None,
                    provenance: None,
                },
            )
            .expect("write child .hcred");

            unsafe { std::env::set_var("HEDDLE_CREDENTIAL", &child_hcred) };
            let explicit_child_session =
                HostedSession::build(&cli_shared::UserConfig::default(), Some(server.to_string()))
                    .expect("build explicit-child session");
            assert_eq!(
                explicit_child_session
                    .config
                    .token
                    .as_ref()
                    .map(|token| token.id.as_str()),
                Some(child_token.as_str()),
                "HEDDLE_CREDENTIAL is authoritative over the keystore"
            );
            assert!(
                explicit_child_session
                    .renewable_authority_credential
                    .is_none(),
                "an env-supplied credential must not borrow the stored parent's renewal identity"
            );
        });
    }

    #[tokio::test]
    async fn token_only_stored_child_does_not_borrow_the_host_device_key() {
        use crypto::{Ed25519Signer, Signer};
        use grpc::heddle::api::v1alpha1::{
            collaboration_service_client::CollaborationServiceClient,
            identity_service_client::IdentityServiceClient,
            registry_service_client::RegistryServiceClient,
            repo_sync_service_client::RepoSyncServiceClient,
            repository_service_client::RepositoryServiceClient,
            state_review_service_client::StateReviewServiceClient,
            workflow_service_client::WorkflowServiceClient,
        };
        use tonic::{Request, metadata::MetadataValue, transport::Endpoint};

        use crate::{auth_cmd, grpc_hosted::HostedGrpcClient};

        with_isolated_env(|home| {
            let signer = Ed25519Signer::generate().expect("device key");
            let private_key_pem = signer.to_pem().expect("device PEM");

            let subject = "headless-agent";
            let credential_id = "cred-headless";
            let expires_at = chrono::Utc::now() + chrono::Duration::days(30);
            let token = biscuit_auth::Biscuit::builder()
                .fact(format!("user(\"{subject}\")").as_str())
                .expect("user fact")
                .fact(format!("credential_id(\"{credential_id}\")").as_str())
                .expect("credential fact")
                .fact(format!("expires_at({})", expires_at.to_rfc3339()).as_str())
                .expect("expiry fact")
                .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
                .expect("PoP key fact")
                .build(&biscuit_auth::KeyPair::new())
                .expect("mint fixture biscuit")
                .to_base64()
                .expect("encode fixture biscuit");
            let server = "127.0.0.1:8421";

            // Install the root device credential: the keystore gets the token
            // AND its device key, and the shared device identity is linked.
            let root_hcred = home.join("root.hcred");
            crate::credential_file::write_credential_file(
                &root_hcred,
                &crate::credential_file::VerifiedCredential {
                    server: server.to_string(),
                    kind: crate::credential_file::CredentialKind::Device,
                    subject: subject.to_string(),
                    token: token.clone(),
                    proof_key_pem: private_key_pem.clone(),
                    expires_at: Some(expires_at.to_rfc3339()),
                    credential_id: Some(credential_id.to_string()),
                    provenance: None,
                },
            )
            .expect("write root .hcred");
            auth_cmd::install_credential_file(&root_hcred).expect("headless credential install");

            let identity = repo::identity::load_device(&repo::identity::device_identity_path())
                .expect("load device identity")
                .expect("linked device identity");
            assert_eq!(identity.public_key, hex::encode(signer.public_key()));
            assert_eq!(identity.server, server);

            // The stored root credential resolves its own proof key directly.
            let root_session =
                HostedSession::build(&cli_shared::UserConfig::default(), Some(server.to_string()))
                    .expect("build root session");
            assert_eq!(
                root_session.config.auth_proof_key_pem.as_deref(),
                Some(private_key_pem.as_str()),
                "a stored root credential resolves its bound device key"
            );

            // Now replace the keystore entry with a TOKEN-ONLY derived child
            // (no proof key of its own). Its PoP key differs from the host
            // device identity, so it must NOT borrow the ancestor device key.
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
            credentials::store_server_credential(
                server,
                credentials::ServerCredential {
                    token: child_token.clone(),
                    subject: subject.to_string(),
                    device_id: None,
                    credential_id: None,
                    private_key_pem: None,
                    expires_at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
                },
            )
            .expect("store token-only child");

            let session =
                HostedSession::build(&cli_shared::UserConfig::default(), Some(server.to_string()))
                    .expect("build token-only child session");
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
    }
}
