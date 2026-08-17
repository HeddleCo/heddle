//! `heddle whoami` — machine-readable acting-identity introspection.
//!
//! Like `auth status`, `whoami` resolves the credential the hosted runtime
//! would actually use. It then calls `IdentityService.WhoAmI`
//! on the server for the authoritative principal/staff/service-account markers
//! and directly-held resource roles, and reads the local Biscuit for the token
//! kind, resource scopes, operation ceiling, and TTL that the delegation chain
//! encodes. The proof-key (signing) availability and TTL remaining are computed
//! locally so the verb still returns useful output when the server is
//! unreachable.

use anyhow::{Context, Result};
use cli_shared::UserConfig;
use crypto::Ed25519Signer;
use serde::Serialize;
use weft_client_shim::CliContext;

use super::{
    auth::{headless_token_metadata, resolve_server},
    hosted::{HostedAuthMode, HostedSession, ResolvedHostedCredential, resolve_hosted_credential},
};

#[derive(Debug, Serialize)]
struct WhoamiOutput {
    output_kind: &'static str,
    server: String,
    /// A usable credential resolves for this server.
    authenticated: bool,
    /// Credential origin: `env:<path>`, `keystore`, or `none`.
    source: String,
    /// Locally verified subject, present even when the server is unreachable.
    subject: Option<String>,
    /// The server answered `WhoAmI` — the acting identity below is authoritative.
    reachable: bool,
    /// `root` (full-authority device/human token), `agent` (an offline-derived,
    /// attenuated delegation), or `service-account`. `None` when unauthenticated.
    token_kind: Option<String>,
    /// Resource scopes the delegation chain restricts this token to, as
    /// `kind:path` (e.g. `repo:alice/api`, `namespace:alice`). Empty ⇒ full
    /// resource authority.
    scopes: Vec<String>,
    /// The intersected hosted-operation ceiling from the delegation chain. `None`
    /// ⇒ no operation allowlist (full authority, minus the mandatory deny floor).
    operation_ceiling: Option<Vec<String>>,
    /// Effective token expiry (RFC3339), the earliest of the authority and every
    /// attenuation hop. `None` ⇒ no expiry recorded.
    expires_at: Option<String>,
    /// Seconds until `expires_at`; negative when already expired.
    ttl_seconds_remaining: Option<i64>,
    /// The device proof key needed to sign hosted requests is present and valid.
    proof_key_available: bool,
    /// Server-authoritative identity, present only when `reachable`.
    identity: Option<WhoamiIdentity>,
    recommended_action: Option<String>,
}

#[derive(Debug, Serialize)]
struct WhoamiIdentity {
    subject: String,
    actor_subject: String,
    is_staff: bool,
    is_service_account: bool,
    is_biscuit: bool,
    session_id: String,
    amr: Vec<String>,
    /// The scope string the server records for this credential.
    server_scope: String,
    credential_id: String,
    device_id: Option<String>,
    agent_provider: Option<String>,
    agent_model: Option<String>,
    /// Resource roles the caller holds directly (UI gating only; the server
    /// enforces effective, inherited roles on each RPC).
    roles: Vec<WhoamiRole>,
}

#[derive(Debug, Serialize)]
struct WhoamiRole {
    resource_path: String,
    resource_kind: String,
    role: String,
}

/// `heddle whoami [--server <addr>]`.
pub async fn cmd_whoami(ctx: &dyn CliContext, server: Option<String>) -> Result<()> {
    let server = resolve_server(server.as_deref())?;
    let output = resolve_whoami(&server).await?;
    if ctx.should_output_json(None) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        print_human(&output);
    }
    Ok(())
}

async fn resolve_whoami(server: &str) -> Result<WhoamiOutput> {
    let resolved = resolve_hosted_credential(Some(server))?;
    let mut output = resolve_local_whoami(server, &resolved)?;
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

fn resolve_local_whoami(server: &str, resolved: &ResolvedHostedCredential) -> Result<WhoamiOutput> {
    let Some(token) = resolved.token.as_ref() else {
        return Ok(WhoamiOutput {
            output_kind: "whoami",
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
fn token_resource_scopes(token: &str) -> Result<Vec<(String, String)>> {
    use biscuit_auth::builder::{BlockBuilder, Term};

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
    use api::heddle::api::v1alpha1::HostedRole;
    match HostedRole::try_from(role) {
        Ok(HostedRole::Reader) => "reader",
        Ok(HostedRole::Developer) => "developer",
        Ok(HostedRole::Maintainer) => "maintainer",
        Ok(HostedRole::Admin) => "admin",
        Ok(HostedRole::Owner) => "owner",
        Ok(HostedRole::Unspecified) | Err(_) => "unspecified",
    }
}

fn print_human(output: &WhoamiOutput) {
    write_human(&mut std::io::stdout().lock(), output).expect("write whoami output");
}

fn write_human(writer: &mut impl std::io::Write, output: &WhoamiOutput) -> std::io::Result<()> {
    writeln!(writer, "Server:        {}", output.server)?;
    if !output.authenticated {
        writeln!(writer, "Not authenticated with {}.", output.server)?;
        if let Some(action) = &output.recommended_action {
            writeln!(writer, "Run `{action}` to authenticate.")?;
        }
        return Ok(());
    }
    writeln!(writer, "Source:        {}", output.source)?;
    if let Some(subject) = &output.subject {
        writeln!(writer, "Subject:       {subject}")?;
    }
    if let Some(identity) = &output.identity {
        if identity.actor_subject != identity.subject && !identity.actor_subject.is_empty() {
            writeln!(writer, "Acting as:     {}", identity.actor_subject)?;
        }
        if !identity.credential_id.is_empty() {
            writeln!(writer, "Credential:    {}", identity.credential_id)?;
        }
        if !identity.session_id.is_empty() {
            writeln!(writer, "Session:       {}", identity.session_id)?;
        }
        if identity.is_staff {
            writeln!(writer, "Staff:         yes")?;
        }
        if !identity.server_scope.is_empty() {
            writeln!(writer, "Server scope:  {}", identity.server_scope)?;
        }
        if !identity.roles.is_empty() {
            let roles = identity
                .roles
                .iter()
                .map(|role| {
                    format!(
                        "{}:{}={}",
                        role.resource_kind, role.resource_path, role.role
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(writer, "Roles:         {roles}")?;
        }
    } else {
        writeln!(
            writer,
            "Server:        unreachable (showing locally-known token facts)"
        )?;
    }
    writeln!(
        writer,
        "Token kind:    {}",
        output.token_kind.as_deref().unwrap_or("unknown")
    )?;
    if output.scopes.is_empty() {
        writeln!(writer, "Scopes:        full resource authority")?;
    } else {
        writeln!(writer, "Scopes:        {}", output.scopes.join(", "))?;
    }
    match &output.operation_ceiling {
        Some(ops) => writeln!(writer, "Op ceiling:    {}", ops.join(", "))?,
        None => writeln!(writer, "Op ceiling:    full (no operation allowlist)")?,
    }
    if let Some(expires_at) = &output.expires_at {
        match output.ttl_seconds_remaining {
            Some(secs) if secs >= 0 => {
                writeln!(writer, "Expires:       {expires_at} (in {secs}s)")?;
            }
            Some(secs) => writeln!(
                writer,
                "Expires:       {expires_at} (EXPIRED {}s ago)",
                -secs
            )?,
            None => writeln!(writer, "Expires:       {expires_at}")?,
        }
    }
    if output.proof_key_available {
        writeln!(writer, "Signing:       ready (device proof key available)")?;
    } else {
        writeln!(writer, "Signing:       unavailable (no device proof key)")?;
    }
    if let Some(action) = &output.recommended_action {
        writeln!(writer, "Note:          run `{action}`.")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosted_runtime::hosted::CredentialSource;

    #[test]
    fn local_unauthenticated_identity_has_actionable_output() {
        let resolved = ResolvedHostedCredential {
            token: None,
            proof_key_pem: None,
            owner_authorization: None,
            renewable: None,
            subject: None,
            credential_id: None,
            expires_at: None,
            source: CredentialSource::Unauthenticated,
        };
        let output = resolve_local_whoami("host.example", &resolved).unwrap();
        assert!(!output.authenticated);
        assert_eq!(output.source, "none");
        assert_eq!(
            output.recommended_action.as_deref(),
            Some("heddle auth login --server host.example")
        );
        let mut rendered = Vec::new();
        write_human(&mut rendered, &output).unwrap();
        assert_eq!(
            String::from_utf8(rendered).unwrap(),
            "Server:        host.example\nNot authenticated with host.example.\n\
             Run `heddle auth login --server host.example` to authenticate.\n"
        );
    }

    #[test]
    fn human_output_renders_authoritative_identity_and_local_token_facts() {
        let mut output = WhoamiOutput {
            output_kind: "whoami",
            server: "host.example".to_string(),
            authenticated: true,
            source: "keystore".to_string(),
            subject: Some("principal:alice".to_string()),
            reachable: true,
            token_kind: Some("agent".to_string()),
            scopes: vec!["repo:alice/widgets".to_string()],
            operation_ceiling: Some(vec!["repo.read".to_string(), "repo.push".to_string()]),
            expires_at: Some("2030-01-01T00:00:00Z".to_string()),
            ttl_seconds_remaining: Some(60),
            proof_key_available: true,
            identity: Some(WhoamiIdentity {
                subject: "principal:alice".to_string(),
                actor_subject: "agent:reviewer".to_string(),
                is_staff: true,
                is_service_account: false,
                is_biscuit: true,
                session_id: "session-1".to_string(),
                amr: vec!["device".to_string()],
                server_scope: "hosted".to_string(),
                credential_id: "credential-1".to_string(),
                device_id: Some("device-1".to_string()),
                agent_provider: Some("openai".to_string()),
                agent_model: Some("codex".to_string()),
                roles: vec![WhoamiRole {
                    resource_path: "alice/widgets".to_string(),
                    resource_kind: "repo".to_string(),
                    role: "maintainer".to_string(),
                }],
            }),
            recommended_action: None,
        };
        let mut rendered = Vec::new();
        write_human(&mut rendered, &output).unwrap();
        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.contains("Acting as:     agent:reviewer"));
        assert!(rendered.contains("Roles:         repo:alice/widgets=maintainer"));
        assert!(rendered.contains("Expires:       2030-01-01T00:00:00Z (in 60s)"));
        output.identity = None;
        output.reachable = false;
        output.scopes.clear();
        output.operation_ceiling = None;
        output.ttl_seconds_remaining = Some(-5);
        output.proof_key_available = false;
        output.recommended_action = Some("heddle auth login --server host.example".to_string());
        let mut rendered = Vec::new();
        write_human(&mut rendered, &output).unwrap();
        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.contains("Server:        unreachable"));
        assert!(rendered.contains("Scopes:        full resource authority"));
        assert!(rendered.contains("EXPIRED 5s ago"));
        output.ttl_seconds_remaining = None;
        let mut rendered = Vec::new();
        write_human(&mut rendered, &output).unwrap();
        assert!(
            String::from_utf8(rendered)
                .unwrap()
                .contains("Expires:       2030-01-01T00:00:00Z\n")
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
}
