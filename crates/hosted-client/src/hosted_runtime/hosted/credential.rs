//! Single credential-resolution precedence for native hosted calls.
//!
//! `HEDDLE_CREDENTIAL=<path.hcred>` is authoritative. If it is absent, the
//! per-server keystore is consulted; otherwise the call is unauthenticated.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use config::credentials;
use wire::AuthToken;

use super::RenewableAuthorityCredential;

const HEDDLE_CREDENTIAL_ENV: &str = "HEDDLE_CREDENTIAL";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    Env(PathBuf),
    Keystore,
    Unauthenticated,
}

impl CredentialSource {
    pub fn label(&self) -> String {
        match self {
            Self::Env(path) => format!("env:{}", path.display()),
            Self::Keystore => "keystore".to_string(),
            Self::Unauthenticated => "none".to_string(),
        }
    }
}

pub struct ResolvedHostedCredential {
    pub mint_root_attachment: Option<Vec<u8>>,
    pub token: Option<AuthToken>,
    pub proof_key_pem: Option<String>,
    pub(crate) renewable: Option<RenewableAuthorityCredential>,
    pub subject: Option<String>,
    pub credential_id: Option<String>,
    pub expires_at: Option<String>,
    pub source: CredentialSource,
}

impl std::fmt::Debug for ResolvedHostedCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedHostedCredential")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field(
                "proof_key_pem",
                &self.proof_key_pem.as_ref().map(|_| "<redacted>"),
            )
            .field("renewable", &self.renewable.is_some())
            .field("subject", &self.subject)
            .field("credential_id", &self.credential_id)
            .field("expires_at", &self.expires_at)
            .field("source", &self.source)
            .finish()
    }
}

fn credential_env_path() -> Result<Option<PathBuf>> {
    match std::env::var(HEDDLE_CREDENTIAL_ENV) {
        Ok(value) => {
            if value.is_empty() {
                anyhow::bail!(
                    "HEDDLE_CREDENTIAL is set but empty; unset it to use the stored credential, \
                     or point it at a .hcred file"
                );
            }
            if value.starts_with('{') || value.contains('\n') {
                anyhow::bail!("HEDDLE_CREDENTIAL takes a file path, not credential contents");
            }
            Ok(Some(PathBuf::from(value)))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error @ std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("HEDDLE_CREDENTIAL is not valid UTF-8: {error}")
        }
    }
}

pub fn resolve_hosted_credential(server_key: Option<&str>) -> Result<ResolvedHostedCredential> {
    if let Some(path) = credential_env_path()? {
        let verified = crate::hosted_runtime::credential_file::load_credential_file(&path)
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
            mint_root_attachment: verified.mint_root_attachment,
            token: Some(AuthToken::new(verified.token, "hcred-env")),
            proof_key_pem: Some(verified.proof_key_pem),
            renewable: None,
            subject: Some(verified.subject),
            credential_id: verified.credential_id,
            expires_at: verified.expires_at,
            source: CredentialSource::Env(path),
        });
    }

    if let Some(key) = server_key
        && let Some(credential) = credentials::resolve_credential_for_server(key)?
    {
        let renewable = RenewableAuthorityCredential::from_stored(&credential);
        return Ok(ResolvedHostedCredential {
            mint_root_attachment: credential.mint_root_attachment,
            token: Some(AuthToken::new(credential.token, "credential-store")),
            proof_key_pem: credential.private_key_pem,
            renewable,
            subject: Some(credential.subject),
            credential_id: credential.credential_id,
            expires_at: credential.expires_at,
            source: CredentialSource::Keystore,
        });
    }

    Ok(ResolvedHostedCredential {
        mint_root_attachment: None,
        token: None,
        proof_key_pem: None,
        renewable: None,
        subject: None,
        credential_id: None,
        expires_at: None,
        source: CredentialSource::Unauthenticated,
    })
}

pub fn resolve_active_bearer() -> Result<Option<AuthToken>> {
    let server = credentials::default_server()?;
    Ok(resolve_hosted_credential(server.as_deref())?.token)
}

/// Resolve run attribution from the locally active, proof-key-held credential.
/// A report's local session label does not authenticate a delegated principal.
pub fn authenticated_run_agent(home: &Path) -> Result<Option<String>> {
    use crypto::{Ed25519Signer, Signer as _};

    let server = credentials::default_server()?;
    let resolved = resolve_hosted_credential(server.as_deref())?;
    let Some(token) = resolved.token else {
        return Ok(None);
    };
    let proof_key = resolved
        .proof_key_pem
        .context("active run credential has no proof key")?;
    let signer = Ed25519Signer::from_pem(&proof_key)?;
    let root = biscuit_verifier::unverified_authority_device_pop_key(&token.id)?
        .context("active run credential has no device root selector")?;
    let parsed = biscuit_verifier::parse_token(&token.id, &[root])?;
    let now = chrono::Utc::now().timestamp();
    let authority = repo::device_authority::load(home, now)?;
    authority.verify_mint_root(&root.to_bytes(), now)?;
    let inspected = biscuit_verifier::inspect_verified_credential(&parsed, &root)?;
    if inspected.expires_at_unix_seconds != 0 && inspected.expires_at_unix_seconds <= now as u64 {
        anyhow::bail!("active run credential expired");
    }
    if inspected
        .revocation_ids
        .iter()
        .any(|id| authority.revoked_ids.contains(id))
    {
        anyhow::bail!("active run credential revoked");
    }
    authority.verify_publisher(&inspected.proof_public_key)?;
    if signer.public_key() != inspected.proof_public_key {
        anyhow::bail!("active run credential proof key differs from bearer");
    }
    Ok(inspected.agent_id)
}

/// Derive a capture principal from a locally stored hosted account.
///
/// The credential subject is usable only when it is itself an email address.
/// A non-email subject (notably an unclaimed `agent-key:*` account) carries no
/// email we can truthfully put on a capture, so it deliberately yields no
/// principal instead of inventing one.
pub fn hosted_account_principal() -> Option<(String, String)> {
    let server = credentials::default_server().ok().flatten().or_else(|| {
        credentials::load_credentials()
            .ok()
            .and_then(|store| store.servers.keys().next().cloned())
    });
    let resolved = resolve_hosted_credential(server.as_deref()).ok()?;
    let subject = resolved
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(principal) = subject.and_then(principal_from_hosted_subject) {
        return Some(principal);
    }
    let server = server.as_deref()?;
    let state = crate::hosted_runtime::identity_state::load()
        .ok()
        .flatten()?;
    if !server_keys_match(&state.server, server) {
        return None;
    }
    let (name, email) = state.claimed_principal()?;
    Some((name.to_string(), email.to_string()))
}

/// Whether the locally authenticated account is still awaiting its human
/// claim ceremony. This is advisory only; the server remains authoritative.
pub fn hosted_account_is_unclaimed() -> bool {
    let Some(server) = credentials::default_server().ok().flatten() else {
        return false;
    };
    let subject_is_agent = resolve_hosted_credential(Some(&server))
        .ok()
        .and_then(|resolved| resolved.subject)
        .is_some_and(|subject| subject.starts_with("agent-key:"));
    if !subject_is_agent {
        return false;
    }
    crate::hosted_runtime::identity_state::load()
        .ok()
        .flatten()
        .filter(|state| server_keys_match(&state.server, &server))
        .is_none_or(|state| !state.consent_issued())
}

pub fn principal_from_hosted_subject(subject: &str) -> Option<(String, String)> {
    let subject = subject.trim();
    if let Some((local, domain)) = subject.split_once('@')
        && !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && !subject.chars().any(char::is_whitespace)
    {
        return Some((local.to_string(), subject.to_string()));
    }
    None
}

pub(crate) fn server_keys_match(left: &str, right: &str) -> bool {
    fn without_scheme(value: &str) -> &str {
        value
            .strip_prefix("http://")
            .or_else(|| value.strip_prefix("https://"))
            .unwrap_or(value)
    }
    without_scheme(left) == without_scheme(right)
}

#[cfg(test)]
mod tests {
    use crypto::{Ed25519Signer, Signer};

    use super::{
        CredentialSource, credential_env_path, resolve_hosted_credential, server_keys_match,
    };

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

    fn write_sample_hcred(path: &std::path::Path, server: &str, subject: &str) {
        let signer = Ed25519Signer::generate().expect("proof key");
        let token = mint_authority_token(subject, &signer);
        crate::hosted_runtime::credential_file::write_credential_file(
            path,
            &crate::hosted_runtime::credential_file::VerifiedCredential {
                mint_root_attachment: None,
                server: server.to_string(),
                kind: crate::hosted_runtime::credential_file::CredentialKind::Device,
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

    fn with_isolated_env<T>(run: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = config::credentials::lock_test_env();
        let home = tempfile::TempDir::new().expect("temp Heddle home");
        let previous_home = std::env::var_os("HEDDLE_HOME");
        let previous_credential = std::env::var_os("HEDDLE_CREDENTIAL");
        unsafe {
            std::env::set_var("HEDDLE_HOME", home.path());
            std::env::remove_var("HEDDLE_CREDENTIAL");
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(home.path())));
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
    fn server_matching_ignores_supported_schemes_only() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        assert!(server_keys_match("https://api.heddle.sh", "api.heddle.sh"));
        assert!(!server_keys_match("api.heddle.sh", "other.heddle.sh"));
    }

    #[test]
    fn inline_credential_contents_are_rejected() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        with_isolated_env(|_| {
            unsafe { std::env::set_var("HEDDLE_CREDENTIAL", "{\"format\":\"heddle-credential\"}") };
            let error = credential_env_path().expect_err("inline contents must not be accepted");
            assert!(error.to_string().contains("takes a file path"));
        });
    }

    #[test]
    fn env_credential_resolves_and_is_not_renewable() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        with_isolated_env(|home| {
            let path = home.join("agent.hcred");
            write_sample_hcred(&path, "api.heddle.test", "alice");
            unsafe { std::env::set_var("HEDDLE_CREDENTIAL", &path) };

            let resolved =
                resolve_hosted_credential(Some("api.heddle.test")).expect("resolve env credential");
            assert!(resolved.token.is_some());
            assert!(resolved.proof_key_pem.is_some());
            assert!(resolved.renewable.is_none());
            assert_eq!(resolved.subject.as_deref(), Some("alice"));
            assert_eq!(resolved.source, CredentialSource::Env(path));
        });
    }

    #[test]
    fn env_server_mismatch_never_falls_back_to_keystore() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        with_isolated_env(|home| {
            config::credentials::store_server_credential(
                "api.target.test",
                config::credentials::ServerCredential {
                    mint_root_attachment: None,
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
            write_sample_hcred(&path, "api.other.test", "agent");
            unsafe { std::env::set_var("HEDDLE_CREDENTIAL", &path) };

            let error = resolve_hosted_credential(Some("api.target.test"))
                .expect_err("server mismatch must be a hard error");
            let message = error.to_string();
            assert!(message.contains("api.other.test"));
            assert!(message.contains("api.target.test"));
        });
    }

    #[test]
    fn unreadable_env_credential_never_falls_back_to_keystore() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        with_isolated_env(|home| {
            config::credentials::store_server_credential(
                "api.target.test",
                config::credentials::ServerCredential {
                    mint_root_attachment: None,
                    token: "keystore-token".to_string(),
                    subject: "human".to_string(),
                    device_id: None,
                    credential_id: None,
                    private_key_pem: None,
                    expires_at: None,
                },
            )
            .expect("seed keystore");
            unsafe { std::env::set_var("HEDDLE_CREDENTIAL", home.join("missing.hcred")) };

            resolve_hosted_credential(Some("api.target.test"))
                .expect_err("unreadable explicit credential must be a hard error");
        });
    }

    #[test]
    fn run_agent_uses_verified_delegation_not_local_session_label() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        with_isolated_env(|home| {
            let root = Ed25519Signer::from_seed(&[71; 32]).expect("owner key");
            let recovery = Ed25519Signer::from_seed(&[72; 32]).expect("recovery key");
            let signed = repo::sign_custodial_owner_root(&root, &recovery, [9; 16], [5; 32])
                .expect("owner root");
            let binding =
                repo::sign_custodial_owner_binding(&root, &signed, [6; 32]).expect("owner binding");
            let version = heddleco_capability_verifier::verify_owner_root(&signed)
                .expect("owner proof")
                .state_hash()
                .to_vec();
            let owner = api::heddle::api::v1alpha2::OwnerState {
                owner: Some(api::heddle::api::v1alpha2::PrincipalRef {
                    id: uuid::Uuid::from_bytes([9; 16]).to_string(),
                }),
                root: Some(signed),
                binding: Some(binding),
                version,
                ..Default::default()
            };
            repo::device_authority::publish(
                home,
                &repo::device_authority::DeviceAuthority {
                    owner,
                    mint_roots: Vec::new(),
                    revoked_ids: Vec::new(),
                    revoked_mint_roots: Vec::new(),
                    revoked_publishers: Vec::new(),
                },
                chrono::Utc::now().timestamp(),
            )
            .expect("admit owner");
            let root_token =
                crate::hosted_runtime::root_mint::mint_agent_root(&[71; 32]).expect("root token");
            let child = Ed25519Signer::from_seed(&[81; 32]).expect("child proof key");
            let token = crate::hosted_runtime::device_flow::attenuate_for_agent(
                &root_token.token,
                crate::hosted_runtime::device_flow::AgentAttenuation::time_bounded(
                    "delegated-agent",
                    chrono::Utc::now() + chrono::Duration::hours(1),
                ),
                &root,
                child.public_key(),
            )
            .expect("delegated token");
            let path = home.join("agent.hcred");
            crate::hosted_runtime::credential_file::write_credential_file(
                &path,
                &crate::hosted_runtime::credential_file::VerifiedCredential {
                    mint_root_attachment: None,
                    server: "api.heddle.test".into(),
                    kind: crate::hosted_runtime::credential_file::CredentialKind::Agent,
                    subject: root_token.subject,
                    token,
                    proof_key_pem: child.to_pem().expect("child PEM"),
                    expires_at: None,
                    credential_id: None,
                    provenance: None,
                },
            )
            .expect("write agent credential");
            unsafe { std::env::set_var("HEDDLE_CREDENTIAL", path) };
            assert_eq!(
                super::authenticated_run_agent(home).expect("authenticated agent"),
                Some("delegated-agent".into())
            );
        });
    }

    #[test]
    fn hosted_subject_requires_an_actual_email() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        assert_eq!(
            super::principal_from_hosted_subject("luke@example.com"),
            Some(("luke".to_string(), "luke@example.com".to_string()))
        );
        assert_eq!(super::principal_from_hosted_subject("luke"), None);
        assert_eq!(super::principal_from_hosted_subject("agent-key:abc"), None);
        assert_eq!(
            super::principal_from_hosted_subject("a@b@example.com"),
            None
        );
    }

    #[test]
    fn hosted_account_principal_reads_stored_login() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        with_isolated_env(|_| {
            config::credentials::store_server_credential(
                "api.heddle.test",
                config::credentials::ServerCredential {
                    mint_root_attachment: None,
                    token: "token".to_string(),
                    subject: "luke@example.com".to_string(),
                    device_id: None,
                    credential_id: None,
                    private_key_pem: None,
                    expires_at: None,
                },
            )
            .expect("store hosted login");
            assert_eq!(
                super::hosted_account_principal(),
                Some(("luke".to_string(), "luke@example.com".to_string()))
            );
        });
    }

    #[test]
    fn hosted_account_principal_uses_claimed_invite_identity() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        with_isolated_env(|_| {
            config::credentials::store_server_credential(
                "api.heddle.test",
                config::credentials::ServerCredential {
                    mint_root_attachment: None,
                    token: "token".to_string(),
                    subject: "agent-key:abc".to_string(),
                    device_id: None,
                    credential_id: None,
                    private_key_pem: None,
                    expires_at: None,
                },
            )
            .expect("store hosted login");
            let mut state = crate::hosted_runtime::identity_state::ClaimState::new(
                "api.heddle.test".into(),
                uuid::Uuid::parse_str("7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28")
                    .expect("account UUID"),
                "agent-key:abc".into(),
                "quiet-otter".into(),
                "11".repeat(32),
                None,
            );
            state.record_account_email(Some("human@example.com".into()));
            assert!(state.reissue(b"claim-secret", 2_000));
            assert!(state.prepare_browser("human-handle", &[1; 32]));
            assert!(state.finish_browser_claim(&[1; 32]));
            crate::hosted_runtime::identity_state::store(&state).expect("store claim state");

            assert_eq!(
                super::hosted_account_principal(),
                Some(("human-handle".into(), "human@example.com".into()))
            );
        });
    }
}
