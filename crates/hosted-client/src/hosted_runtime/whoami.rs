//! `heddle whoami` — capture actor first, hosted auth second.
//!
//! The capture actor is who the next capture is attributed to
//! (`user_config`, `init --principal-*`, environment). Hosted auth is
//! whether this machine has a server credential. These are different
//! objects. `heddle auth login` does not set the local actor. `whoami`
//! only reads; it never attaches a credential.

use anyhow::{Context, Result};
use api::heddle::api::v1alpha2::{CredentialKind, RootingTier};
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
    /// Grant-reachable hosted paths as `spool/<handle>/<name>`.
    /// Empty when unauthenticated, unreachable, or ListSpools was unavailable.
    pub spools: Vec<String>,
    pub recommended_action: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WhoamiIdentity {
    pub principal_id: String,
    pub account_id: String,
    pub handle: Option<String>,
    pub acting_agent_id: Option<String>,
    pub rooting_tier: String,
    pub credential_id: Option<String>,
    pub credential_subject: String,
    pub credential_kind: String,
    pub session_id: Option<String>,
    pub authentication_methods: Vec<String>,
    pub agent_provider: Option<String>,
    pub agent_model: Option<String>,
    /// Effective method hints from the current credential observation, not
    /// guessed resource roles or authority granted by this read.
    pub available_actions: Vec<String>,
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

    // Server round trip for the authoritative identity and grant-reachable
    // Spool set. Failure (unreachable, rejected, or missing proof key)
    // degrades to a local-only answer rather than erroring — `reachable`
    // records which case this is. ListSpools is additive: an older server
    // still yields identity without failing whoami.
    match fetch_identity(server).await {
        Ok((identity, spools)) => {
            output.token_kind = Some(identity.credential_kind.clone());
            output.identity = Some(identity);
            output.spools = spools;
            output.reachable = true;
        }
        Err(_) => {
            output.reachable = false;
        }
    }
    output.recommended_action = if !output.proof_key_available {
        Some(format!("heddle auth login --server {server}"))
    } else if !output.reachable {
        Some(format!(
            "server did not answer ObserveIdentity; check connectivity to {server} or re-run `heddle auth login --server {server}`"
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
    let hosted = super::hosted::hosted_account_principal();
    let resolved = verbs::apply_hosted_principal_fallback(
        resolved,
        hosted
            .as_ref()
            .map(|(name, email)| (name.as_str(), email.as_str())),
    );
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
            spools: Vec::new(),
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
            "server did not answer ObserveIdentity; check connectivity to {server} or re-run `heddle auth login --server {server}`"
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
        spools: Vec::new(),
        recommended_action,
    })
}

async fn fetch_identity(server: &str) -> Result<(WhoamiIdentity, Vec<String>)> {
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
    let result = async {
        let (principal, credential) = client
            .observe_current_identity()
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
        let identity = project_current_identity(principal, credential)?;
        let spools = match client.list_spools(false).await {
            Ok(rows) => {
                let mut paths = rows
                    .into_iter()
                    .filter_map(|spool| listed_spool_path(&spool.path_segments))
                    .collect::<Vec<_>>();
                paths.sort();
                paths.dedup();
                paths
            }
            Err(_) => Vec::new(),
        };
        Ok((identity, spools))
    }
    .await;
    client.close().await;
    result
}

fn listed_spool_path(path_segments: &[String]) -> Option<String> {
    if path_segments.is_empty()
        || path_segments
            .iter()
            .any(|segment| segment.is_empty() || segment.contains('/'))
    {
        return None;
    }
    let path = path_segments.join("/");
    Some(if path == "spool" || path.starts_with("spool/") {
        path
    } else {
        format!("spool/{path}")
    })
}

fn project_current_identity(
    principal: api::heddle::api::v1alpha2::PrincipalRecord,
    credential: api::heddle::api::v1alpha2::CurrentCredentialRecord,
) -> Result<WhoamiIdentity> {
    let kind = match CredentialKind::try_from(credential.kind).ok() {
        Some(CredentialKind::Device) => "device",
        Some(CredentialKind::Agent) => "agent",
        Some(CredentialKind::Service) => "service",
        Some(CredentialKind::Anonymous) => "anonymous",
        _ => anyhow::bail!("identity view has unknown credential kind"),
    };
    let rooting_tier = match RootingTier::try_from(principal.rooting_tier).ok() {
        Some(RootingTier::SelfRooted) => "self-rooted",
        Some(RootingTier::ServerRooted) => "server-rooted",
        Some(RootingTier::AgentRooted) => "agent-rooted",
        _ => anyhow::bail!("identity view has unknown account rooting tier"),
    };
    let mut actions = credential
        .actions
        .into_iter()
        .filter(|action| action.implemented && action.authorized)
        .map(|action| {
            if action.target.is_some() {
                format!("{} [target-scoped]", action.method)
            } else {
                action.method
            }
        })
        .collect::<Vec<_>>();
    actions.sort();
    actions.dedup();
    Ok(WhoamiIdentity {
        principal_id: principal.id,
        account_id: principal.account_id,
        handle: (!principal.handle.is_empty()).then_some(principal.handle),
        acting_agent_id: (!principal.acting_agent_id.is_empty())
            .then_some(principal.acting_agent_id),
        rooting_tier: rooting_tier.into(),
        credential_id: credential.r#ref.map(|value| value.id),
        credential_subject: credential.subject,
        credential_kind: kind.into(),
        session_id: credential
            .session
            .and_then(|session| session.r#ref.map(|value| value.id)),
        authentication_methods: credential.authentication_methods,
        agent_provider: (!credential.agent_provider.is_empty())
            .then_some(credential.agent_provider),
        agent_model: (!credential.agent_model.is_empty()).then_some(credential.agent_model),
        available_actions: actions,
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

#[cfg(test)]
mod tests {
    use api::heddle::api::v1alpha2::{
        ActionAvailability, CurrentCredentialRecord, EntityRef, PrincipalRecord, RecordRef,
    };
    use objects::object::Principal;

    use super::*;
    use crate::hosted_runtime::hosted::CredentialSource;

    #[test]
    fn native_whoami_keeps_account_actor_and_effective_methods_distinct() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        let principal = PrincipalRecord {
            id: "principal-1".into(),
            account_id: "account-1".into(),
            rooting_tier: RootingTier::SelfRooted as i32,
            ..Default::default()
        };
        let credential = CurrentCredentialRecord {
            r#ref: Some(RecordRef {
                id: "credential-1".into(),
                ..Default::default()
            }),
            kind: CredentialKind::Agent as i32,
            subject: "agent:reviewer".into(),
            acting_agent_id: "reviewer".into(),
            actions: vec![
                ActionAvailability {
                    method: "RecordReview".into(),
                    implemented: true,
                    authorized: true,
                    ..Default::default()
                },
                ActionAvailability {
                    method: "PutGrant".into(),
                    implemented: true,
                    authorized: false,
                    ..Default::default()
                },
                ActionAvailability {
                    method: "RevokeSession".into(),
                    implemented: true,
                    authorized: true,
                    target: Some(EntityRef::default()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let viewed = project_current_identity(principal.clone(), credential.clone())
            .expect("native projection");
        assert_eq!(viewed.account_id, "account-1");
        assert_eq!(viewed.credential_subject, "agent:reviewer");
        assert_eq!(viewed.credential_kind, "agent");
        assert_eq!(
            viewed.available_actions,
            ["RecordReview", "RevokeSession [target-scoped]"]
        );
        let mut invalid = credential;
        invalid.kind = CredentialKind::Unspecified as i32;
        assert!(
            project_current_identity(principal, invalid).is_err(),
            "unknown credential classes cannot masquerade as a root"
        );
    }

    fn luke_actor() -> CaptureActor {
        capture_actor_from_resolved(&ResolvedPrincipal {
            principal: Principal::new("Luke", "luke@example.com"),
            source: Some("user_config"),
        })
    }

    #[test]
    fn capture_actor_from_resolved_maps_user_config() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        let actor = luke_actor();
        assert_eq!(actor.name, "Luke");
        assert_eq!(actor.email, "luke@example.com");
        assert_eq!(actor.source, Some("user_config"));
    }

    #[test]
    fn without_repo_uses_user_config_when_env_unset() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
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
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
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
    fn biscuit_literal_helper_covers_escaped_values() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        assert_eq!(
            biscuit_string_literals(r#"check if operation($op), $op == "repo.read""#),
            vec!["repo.read".to_string()]
        );
    }

    #[test]
    fn listed_spool_path_is_spool_handle_name() {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        assert_eq!(
            listed_spool_path(&["spool".into(), "acme".into(), "notes".into()]),
            Some("spool/acme/notes".into())
        );
        assert_eq!(
            listed_spool_path(&["acme".into(), "notes".into()]),
            Some("spool/acme/notes".into())
        );
        assert_eq!(listed_spool_path(&[]), None);
        assert_eq!(listed_spool_path(&["spool".into(), String::new()]), None);
        assert_eq!(listed_spool_path(&["spool/acme".into()]), None);
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
