use anyhow::{Context, Result};
use api::heddle::api::v1alpha1::HostedRole;
use biscuit_auth::builder::{BlockBuilder, Term};
use cli_shared::UserConfig;
use crypto::Ed25519Signer;

use super::report::{CaptureActor, WhoamiIdentity, WhoamiOutput, WhoamiRole};
use crate::hosted_runtime::{
    auth::headless_token_metadata,
    hosted::{HostedAuthMode, HostedSession, ResolvedHostedCredential},
};

pub(super) fn resolve_local_whoami(
    server: &str,
    resolved: &ResolvedHostedCredential,
    capture_actor: CaptureActor,
) -> Result<WhoamiOutput> {
    let Some(token) = resolved.token.as_ref() else {
        return Ok(WhoamiOutput {
            output_kind: "whoami",
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

    // Local Biscuit introspection — works with no server round trip.
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

    Ok(WhoamiOutput {
        output_kind: "whoami",
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

pub(super) async fn fetch_identity(server: &str) -> Result<WhoamiIdentity> {
    let user_config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &user_config,
        Some(server.to_string()),
        HostedAuthMode::CredentialFallback,
    )
    .map_err(|error| anyhow::anyhow!(error))?;
    let mut client = session
        .connect(([127, 0, 0, 1], 0).into())
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

/// Resource scopes declared by a token's attenuation chain, read from the
/// `agent_scope(kind, path)` facts each derivation hop records. Returned in
/// first-seen order with duplicates removed. Empty for a full-authority
/// (unattenuated) token.
pub(super) fn token_resource_scopes(token: &str) -> Result<Vec<(String, String)>> {
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

/// The effective hosted-operation ceiling for a token: the INTERSECTION of every
/// `check if operation($op), $op == …` allowlist across the attenuation chain
/// (each hop can only narrow). `None` means no operation allowlist is present —
/// i.e. full-authority for operations (the mandatory deny floor still applies).
pub(super) fn token_operation_ceiling(token: &str) -> Result<Option<Vec<String>>> {
    let biscuit = biscuit_auth::UnverifiedBiscuit::from_base64(token.as_bytes())
        .context("parsing Biscuit token operation ceiling")?;
    let mut intersection: Option<std::collections::BTreeSet<String>> = None;
    for index in 1..biscuit.block_count() {
        let source = biscuit
            .print_block_source(index)
            .with_context(|| format!("reading Biscuit attenuation block {index}"))?;
        for statement in source.split(';') {
            let statement = statement.trim();
            // Only positive operation allowlists narrow the ceiling. Skip the
            // mandatory deny floor (`$op != "…"`) and any non-operation check.
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

/// Extract the string literals (`"…"`) from a fragment of Biscuit DSL. Only the
/// CLI-emitted, allowlist-validated shapes are parsed, so minimal escape
/// handling (`\"`, `\\`) is sufficient.
pub(super) fn biscuit_string_literals(fragment: &str) -> Vec<String> {
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

pub(super) fn hosted_role_name(role: i32) -> &'static str {
    match HostedRole::try_from(role) {
        Ok(HostedRole::Reader) => "reader",
        Ok(HostedRole::Developer) => "developer",
        Ok(HostedRole::Maintainer) => "maintainer",
        Ok(HostedRole::Admin) => "admin",
        Ok(HostedRole::Owner) => "owner",
        Ok(HostedRole::Unspecified) | Err(_) => "unspecified",
    }
}
