//! `heddle auth login` path selection: the only hosted-auth write.

use anyhow::{Context, Result};
use chrono::Utc;
use cli_shared::credentials;
use crypto::{Ed25519Signer, Signer as _};
use heddle_cli_contract::cli::commands::RecoveryAdvice;
use weft_client_shim::CliContext;

use super::{
    agent_node_identity,
    auth::{cmd_auth_login_browser, headless_token_metadata},
    hosted::{ResolvedHostedCredential, resolve_hosted_credential},
    identity_state,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoginPath {
    Reuse,
    Remint,
    CreateWithInvite,
    Browser,
    FailClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LoginInputs {
    pub reusable_cred: bool,
    pub node_key_account: bool,
    pub has_invite: bool,
    pub interactive: bool,
    pub force_browser: bool,
}

pub(crate) fn login_path(inputs: LoginInputs) -> LoginPath {
    if inputs.reusable_cred {
        LoginPath::Reuse
    } else if inputs.node_key_account {
        LoginPath::Remint
    } else if inputs.has_invite {
        LoginPath::CreateWithInvite
    } else if inputs.interactive || inputs.force_browser {
        LoginPath::Browser
    } else {
        LoginPath::FailClosed
    }
}

pub(crate) async fn login(
    ctx: &dyn CliContext,
    server: &str,
    open_browser: bool,
    invite: Option<String>,
    interactive: bool,
) -> Result<()> {
    let resolved = resolve_hosted_credential(Some(server))?;
    let reusable_cred = credential_is_reusable(&resolved);
    let node_key_account = node_key_account_exists(server, &resolved)?;
    match login_path(LoginInputs {
        reusable_cred,
        node_key_account,
        has_invite: invite.is_some(),
        interactive,
        force_browser: open_browser,
    }) {
        LoginPath::Reuse => reuse(server, &resolved),
        LoginPath::Remint => super::auth_login_agent::remint(server).await,
        LoginPath::CreateWithInvite => {
            let invite = invite.context("login decision lost invite")?;
            super::auth_login_agent::create_with_invite(ctx, server, invite).await
        }
        LoginPath::Browser => cmd_auth_login_browser(server, open_browser).await,
        LoginPath::FailClosed => fail_closed(server),
    }
}

fn credential_is_reusable(resolved: &ResolvedHostedCredential) -> bool {
    if resolved.token.is_none() {
        return false;
    }
    match resolved.expires_at.as_deref() {
        None => true,
        Some(value) => chrono::DateTime::parse_from_rfc3339(value)
            .map(|parsed| parsed.with_timezone(&Utc) > Utc::now())
            .unwrap_or(false),
    }
}

fn node_key_account_exists(server: &str, resolved: &ResolvedHostedCredential) -> Result<bool> {
    let Some(identity) = agent_node_identity::load()? else {
        return Ok(false);
    };
    let node_id = identity.node_id().to_string();
    if cred_bound_to_node_key(resolved, &node_id) {
        return Ok(true);
    }
    Ok(identity_state::load()?.is_some_and(|state| {
        state.server == server && state.node_id.eq_ignore_ascii_case(&node_id)
    }))
}

fn cred_bound_to_node_key(resolved: &ResolvedHostedCredential, node_id: &str) -> bool {
    if let Some(pem) = resolved.proof_key_pem.as_deref()
        && let Ok(signer) = Ed25519Signer::from_pem(pem)
        && hex::encode(signer.public_key()).eq_ignore_ascii_case(node_id)
    {
        return true;
    }
    resolved
        .token
        .as_ref()
        .and_then(|token| headless_token_metadata(&token.id).ok())
        .is_some_and(|metadata| metadata.proof_public_key_hex.eq_ignore_ascii_case(node_id))
}

fn reuse(server: &str, resolved: &ResolvedHostedCredential) -> Result<()> {
    let subject = resolved
        .subject
        .clone()
        .or_else(|| {
            resolved
                .token
                .as_ref()
                .and_then(|token| headless_token_metadata(&token.id).ok())
                .map(|metadata| metadata.subject)
        })
        .unwrap_or_else(|| server.to_string());
    print_reused(&subject);
    Ok(())
}

fn fail_closed(server: &str) -> Result<()> {
    let primary = "heddle auth login --invite <code>".to_string();
    Err(anyhow::Error::new(RecoveryAdvice::safety_refusal(
        "auth_login_invite_required",
        format!("Not authenticated with {server}"),
        format!("Run `{primary}` to create an account, then retry."),
        "this session has no TTY and no reusable credential or hosted account for this node key",
        "starting a browser or Iroh claim wait would hang until a human opens a URL",
        "no hosted request was sent and local credentials were left unchanged",
        primary.clone(),
        vec![primary],
    )))
}

pub(crate) fn print_reused(subject: &str) {
    println!("Authenticated as {subject}.");
}

pub(crate) fn print_success(subject: &str) {
    println!("Authenticated as {subject}. Credentials saved.");
}

pub(crate) fn print_claim_link(claim_link: &str) {
    println!("\nOpen this short-lived claim link:\n\n{claim_link}\n");
}

pub(crate) fn store_agent_root(
    server: &str,
    token: String,
    subject: String,
    private_key_pem: String,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    credentials::store_server_credential(
        server,
        credentials::ServerCredential {
            token,
            subject,
            device_id: None,
            credential_id: None,
            private_key_pem: Some(private_key_pem),
            expires_at: Some(expires_at.to_rfc3339()),
        },
    )
}
