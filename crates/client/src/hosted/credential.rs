//! Single credential-resolution precedence for native hosted calls.
//!
//! `HEDDLE_CREDENTIAL=<path.hcred>` is authoritative. If it is absent, the
//! per-server keystore is consulted; otherwise the call is unauthenticated.

use std::path::PathBuf;

use anyhow::{Context, Result};
use wire::AuthToken;

use super::RenewableAuthorityCredential;
use crate::credentials;

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

    if let Some(key) = server_key
        && let Some(credential) = credentials::resolve_credential_for_server(key)?
    {
        let renewable = RenewableAuthorityCredential::from_stored(&credential);
        return Ok(ResolvedHostedCredential {
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

pub(super) fn server_keys_match(left: &str, right: &str) -> bool {
    fn without_scheme(value: &str) -> &str {
        value
            .strip_prefix("http://")
            .or_else(|| value.strip_prefix("https://"))
            .or_else(|| value.strip_prefix("heddle://"))
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

    fn with_isolated_env<T>(run: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = crate::credentials::lock_test_env();
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
        assert!(server_keys_match("https://api.heddle.sh", "api.heddle.sh"));
        assert!(!server_keys_match("api.heddle.sh", "other.heddle.sh"));
    }

    #[test]
    fn inline_credential_contents_are_rejected() {
        with_isolated_env(|_| {
            unsafe { std::env::set_var("HEDDLE_CREDENTIAL", "{\"format\":\"heddle-credential\"}") };
            let error = credential_env_path().expect_err("inline contents must not be accepted");
            assert!(error.to_string().contains("takes a file path"));
        });
    }

    #[test]
    fn env_credential_resolves_and_is_not_renewable() {
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
        with_isolated_env(|home| {
            crate::credentials::store_server_credential(
                "api.target.test",
                crate::credentials::ServerCredential {
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
        with_isolated_env(|home| {
            crate::credentials::store_server_credential(
                "api.target.test",
                crate::credentials::ServerCredential {
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
}
