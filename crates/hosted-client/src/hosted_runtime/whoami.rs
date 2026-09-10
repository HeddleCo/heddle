//! `heddle whoami` — capture actor first, hosted auth second.
//!
//! The capture actor is who the next capture is attributed to
//! (`user_config`, `init --principal-*`, environment). Hosted auth is
//! whether this machine has a server credential. These are different
//! objects. `heddle auth login` does not set the local actor. `whoami`
//! only reads; it never attaches a credential.

use anyhow::{Context, Result};
use api::heddle::api::v1alpha1::HostedRole;
use biscuit_auth::builder::{BlockBuilder, Term};
use config::UserConfig;
use crypto::Ed25519Signer;
use repo::Repository;
use verbs::{ResolvedPrincipal, resolve_principal, resolve_principal_without_repo};

use super::{
    auth::{headless_token_metadata, resolve_server},
    hosted::{HostedAuthMode, HostedSession, ResolvedHostedCredential, resolve_hosted_credential},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureActor {
    pub name: String,
    pub email: String,
    pub source: Option<&'static str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WhoamiReport {
    pub capture_actor: CaptureActor,
    pub server: String,
    pub authenticated: bool,
    pub source: String,
    pub subject: Option<String>,
    pub reachable: bool,
    pub token_kind: Option<String>,
    pub scopes: Vec<String>,
    pub operation_ceiling: Option<Vec<String>>,
    pub expires_at: Option<String>,
    pub ttl_seconds_remaining: Option<i64>,
    pub proof_key_available: bool,
    pub identity: Option<WhoamiIdentity>,
    pub recommended_action: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WhoamiIdentity {
    pub subject: String,
    pub actor_subject: String,
    pub is_staff: bool,
    pub is_service_account: bool,
    pub is_biscuit: bool,
    pub session_id: String,
    pub amr: Vec<String>,
    pub server_scope: String,
    pub credential_id: String,
    pub device_id: Option<String>,
    pub agent_provider: Option<String>,
    pub agent_model: Option<String>,
    pub roles: Vec<WhoamiRole>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WhoamiRole {
    pub resource_path: String,
    pub resource_kind: String,
    pub role: String,
}

fn capture_actor_from_resolved(resolved: &ResolvedPrincipal) -> CaptureActor {
    CaptureActor {
        name: resolved.principal.name_lossy().into_owned(),
        email: resolved.principal.email_lossy().into_owned(),
        source: resolved.source,
    }
}

/// Resolve local capture attribution and hosted identity without rendering.
pub async fn whoami(start_path: &std::path::Path, server: Option<&str>) -> Result<WhoamiReport> {
    let server = resolve_server(server)?;
    resolve_whoami(start_path, &server).await
}

async fn resolve_whoami(start_path: &std::path::Path, server: &str) -> Result<WhoamiReport> {
    let capture_actor = resolve_capture_actor(start_path)?;
    let resolved = resolve_hosted_credential(Some(server))?;
    let mut output = resolve_local_whoami(server, &resolved, capture_actor)?;
    if !output.authenticated {
        return Ok(output);
    }

    // Server round trip for the authoritative identity. Failure (unreachable,
    // rejected, or missing proof key) degrades to a local-only answer rather
    // than erroring — `reachable` records which case this is.
    output.identity = fetch_identity(server).await.ok();
    output.reachable = output.identity.is_some();
    if output
        .identity
        .as_ref()
        .is_some_and(|identity| identity.is_service_account)
    {
        output.token_kind = Some("service-account".to_string());
    }
    output.recommended_action = if !output.proof_key_available {
        Some(format!("heddle auth login --server {server}"))
    } else if !output.reachable {
        Some(format!(
            "server did not answer WhoAmI; check connectivity to {server} or re-run `heddle auth login --server {server}`"
        ))
    } else {
        None
    };
    Ok(output)
}

/// Who the next capture is attributed to.
///
/// Observe-only: [`Repository::open_existing`] probes with
/// [`repo::discover_heddle_root`] and opens only an already-present store.
/// [`Repository::open`] on a plain Git tree would bootstrap a `.heddle`
/// sidecar and rewrite Git excludes. A discovered store that fails to open
/// is surfaced, not rewritten as "no repository."
fn resolve_capture_actor(start: &std::path::Path) -> Result<CaptureActor> {
    let user_config = UserConfig::load_default()?;
    let resolved = match Repository::open_existing(start)
        .with_context(|| format!("open Heddle store at {}", start.display()))?
    {
        Some(repo) => resolve_principal(&repo, user_config.principal_pair())?,
        None => resolve_principal_without_repo(user_config.principal_pair()),
    };
    Ok(capture_actor_from_resolved(&resolved))
}

fn resolve_local_whoami(
    server: &str,
    resolved: &ResolvedHostedCredential,
    capture_actor: CaptureActor,
) -> Result<WhoamiReport> {
    let Some(token) = resolved.token.as_ref() else {
        return Ok(WhoamiReport {
            capture_actor,
            server: server.to_string(),
            authenticated: false,
            source: resolved.source.label(),
            subject: None,
            reachable: false,
            token_kind: None,
            scopes: Vec::new(),
            operation_ceiling: None,
            expires_at: None,
            ttl_seconds_remaining: None,
            proof_key_available: false,
            identity: None,
            recommended_action: Some(format!("heddle auth login --server {server}")),
        });
    };

    let proof_key_available = resolved
        .proof_key_pem
        .as_deref()
        .is_some_and(|pem| Ed25519Signer::from_pem(pem).is_ok());

    let metadata =
        headless_token_metadata(&token.id).context("reading the active credential's Biscuit")?;
    let scopes = token_resource_scopes(&token.id)
        .context("reading the token's resource scopes")?
        .into_iter()
        .map(|(kind, path)| format!("{kind}:{path}"))
        .collect::<Vec<_>>();
    let operation_ceiling =
        token_operation_ceiling(&token.id).context("reading the token's operation ceiling")?;
    let expires_at = metadata.expires_at.clone();
    let ttl_seconds_remaining = expires_at.as_deref().and_then(|value| {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|expiry| (expiry.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds())
    });

    let token_kind = Some(if metadata.is_derived { "agent" } else { "root" }.to_string());

    let recommended_action = if !proof_key_available {
        Some(format!("heddle auth login --server {server}"))
    } else {
        Some(format!(
            "server did not answer WhoAmI; check connectivity to {server} or re-run `heddle auth login --server {server}`"
        ))
    };

    Ok(WhoamiReport {
        capture_actor,
        server: server.to_string(),
        authenticated: true,
        source: resolved.source.label(),
        subject: resolved.subject.clone(),
        reachable: false,
        token_kind,
        scopes,
        operation_ceiling,
        expires_at,
        ttl_seconds_remaining,
        proof_key_available,
        identity: None,
        recommended_action,
    })
}

async fn fetch_identity(server: &str) -> Result<WhoamiIdentity> {
    let user_config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &user_config,
        Some(server.to_string()),
        HostedAuthMode::CredentialFallback,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    let mut client = session
        .connect(server)
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    let response = client
        .who_am_i()
        .await
        .map_err(|error| anyhow::anyhow!(error));
    client.close().await;
    let response = response?;
    Ok(WhoamiIdentity {
        subject: response.subject,
        actor_subject: response.actor_subject,
        is_staff: response.is_staff,
        is_service_account: response.is_service_account,
        is_biscuit: response.is_biscuit,
        session_id: response.session_id,
        amr: response.amr,
        server_scope: response.scope,
        credential_id: response.credential_id,
        device_id: response.device_id,
        agent_provider: response.agent_provider,
        agent_model: response.agent_model,
        roles: response
            .roles
            .into_iter()
            .map(|role| WhoamiRole {
                resource_path: role.resource_path,
                resource_kind: role.resource_kind,
                role: hosted_role_name(role.role).to_string(),
            })
            .collect(),
    })
}

fn token_resource_scopes(token: &str) -> Result<Vec<(String, String)>> {
    let biscuit = biscuit_auth::UnverifiedBiscuit::from_base64(token.as_bytes())
        .context("parsing Biscuit token scopes")?;
    let mut seen = std::collections::BTreeSet::new();
    let mut scopes = Vec::new();
    for index in 1..biscuit.block_count() {
        let source = biscuit
            .print_block_source(index)
            .with_context(|| format!("reading Biscuit attenuation block {index}"))?;
        let block = BlockBuilder::new()
            .code(&source)
            .with_context(|| format!("parsing Biscuit attenuation block {index}"))?;
        for fact in &block.facts {
            if fact.predicate.name != "agent_scope" || fact.predicate.terms.len() != 2 {
                continue;
            }
            if let (Term::Str(kind), Term::Str(path)) =
                (&fact.predicate.terms[0], &fact.predicate.terms[1])
                && seen.insert((kind.clone(), path.clone()))
            {
                scopes.push((kind.clone(), path.clone()));
            }
        }
    }
    Ok(scopes)
}

fn token_operation_ceiling(token: &str) -> Result<Option<Vec<String>>> {
    let biscuit = biscuit_auth::UnverifiedBiscuit::from_base64(token.as_bytes())
        .context("parsing Biscuit token operation ceiling")?;
    let mut intersection: Option<std::collections::BTreeSet<String>> = None;
    for index in 1..biscuit.block_count() {
        let source = biscuit
            .print_block_source(index)
            .with_context(|| format!("reading Biscuit attenuation block {index}"))?;
        for statement in source.split(';') {
            let statement = statement.trim();
            if !statement.contains("operation($op)") || !statement.contains("$op ==") {
                continue;
            }
            let ops: std::collections::BTreeSet<String> =
                biscuit_string_literals(statement).into_iter().collect();
            intersection = Some(match intersection {
                Some(existing) => existing.intersection(&ops).cloned().collect(),
                None => ops,
            });
        }
    }
    Ok(intersection.map(|ops| ops.into_iter().collect()))
}

fn biscuit_string_literals(fragment: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut chars = fragment.chars();
    while let Some(ch) = chars.next() {
        if ch != '"' {
            continue;
        }
        let mut literal = String::new();
        while let Some(inner) = chars.next() {
            match inner {
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        literal.push(escaped);
                    }
                }
                '"' => break,
                _ => literal.push(inner),
            }
        }
        literals.push(literal);
    }
    literals
}

fn hosted_role_name(role: i32) -> &'static str {
    match HostedRole::try_from(role) {
        Ok(HostedRole::Reader) => "reader",
        Ok(HostedRole::Developer) => "developer",
        Ok(HostedRole::Maintainer) => "maintainer",
        Ok(HostedRole::Admin) => "admin",
        Ok(HostedRole::Owner) => "owner",
        Ok(HostedRole::Unspecified) | Err(_) => "unspecified",
    }
}

#[cfg(test)]
mod tests {
    use objects::object::Principal;

    use super::*;
    use crate::hosted_runtime::hosted::CredentialSource;

    fn luke_actor() -> CaptureActor {
        capture_actor_from_resolved(&ResolvedPrincipal {
            principal: Principal::new("Luke", "luke@example.com"),
            source: Some("user_config"),
        })
    }

    #[test]
    fn capture_actor_from_resolved_maps_user_config() {
        let actor = luke_actor();
        assert_eq!(actor.name, "Luke");
        assert_eq!(actor.email, "luke@example.com");
        assert_eq!(actor.source, Some("user_config"));
    }

    #[test]
    fn without_repo_uses_user_config_when_env_unset() {
        let _guard = PrincipalEnvGuard::clear();
        let user_config = UserConfig {
            principal: Some(config::config::UserPrincipalConfig {
                name: "Luke".to_string(),
                email: "luke@example.com".to_string(),
            }),
            ..UserConfig::default()
        };
        let resolved = resolve_principal_without_repo(user_config.principal_pair());
        assert_eq!(resolved.source, Some("user_config"));
        assert_eq!(resolved.principal.name_lossy(), "Luke");
        assert_eq!(resolved.principal.email_lossy(), "luke@example.com");
    }

    #[test]
    fn local_unauthenticated_identity_has_actionable_output() {
        let resolved = ResolvedHostedCredential {
            mint_root_attachment: None,
            token: None,
            proof_key_pem: None,
            renewable: None,
            subject: None,
            credential_id: None,
            expires_at: None,
            source: CredentialSource::Unauthenticated,
        };
        let output = resolve_local_whoami("host.example", &resolved, luke_actor()).unwrap();
        assert!(!output.authenticated);
        assert_eq!(output.source, "none");
        assert_eq!(output.capture_actor.name, "Luke");
        assert_eq!(output.capture_actor.source, Some("user_config"));
        assert_eq!(
            output.recommended_action.as_deref(),
            Some("heddle auth login --server host.example")
        );
    }

    #[test]
    fn biscuit_literal_and_role_helpers_cover_escaped_and_unknown_values() {
        assert_eq!(
            biscuit_string_literals(r#"check if operation($op), $op == "repo.read""#),
            vec!["repo.read".to_string()]
        );
        assert_eq!(hosted_role_name(1), "reader");
        assert_eq!(hosted_role_name(2), "developer");
        assert_eq!(hosted_role_name(3), "maintainer");
        assert_eq!(hosted_role_name(4), "admin");
        assert_eq!(hosted_role_name(5), "owner");
        assert_eq!(hosted_role_name(i32::MAX), "unspecified");
    }

    struct PrincipalEnvGuard {
        name: Option<std::ffi::OsString>,
        email: Option<std::ffi::OsString>,
    }

    impl PrincipalEnvGuard {
        fn clear() -> Self {
            let name = std::env::var_os("HEDDLE_PRINCIPAL_NAME");
            let email = std::env::var_os("HEDDLE_PRINCIPAL_EMAIL");
            unsafe {
                std::env::remove_var("HEDDLE_PRINCIPAL_NAME");
                std::env::remove_var("HEDDLE_PRINCIPAL_EMAIL");
            }
            Self { name, email }
        }
    }

    impl Drop for PrincipalEnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.name {
                    Some(value) => std::env::set_var("HEDDLE_PRINCIPAL_NAME", value),
                    None => std::env::remove_var("HEDDLE_PRINCIPAL_NAME"),
                }
                match &self.email {
                    Some(value) => std::env::set_var("HEDDLE_PRINCIPAL_EMAIL", value),
                    None => std::env::remove_var("HEDDLE_PRINCIPAL_EMAIL"),
                }
            }
        }
    }
}
