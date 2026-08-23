//! Reuse-first machine identity and browser claim-link commands.

use anyhow::{Context, Result, bail};
use api::heddle::api::v1alpha1::CreateAgentAccountRequest;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use config::{UserConfig, credentials::ServerCredential};
use crypto::{Ed25519Signer, Signer as _};
use heddle_cli_args::CliContext;

use super::{
    HostedAuthMode, HostedSession, agent_node_identity,
    auth::{HeadlessTokenMetadata, cmd_auth_derive_agent, headless_token_metadata, resolve_server},
    device_flow::restrict_agent_account_root,
    hosted::{canonical_server_authority, resolve_hosted_credential},
    identity_server,
    identity_state::{self, ClaimState},
    root_mint::{is_local_agent_root, local_agent_credential_needs_refresh, mint_agent_root},
};
use heddle_cli_args::IdentityCommands;

const DEFAULT_AGENT_TTL_SECS: u64 = 60 * 60;
const MAX_CLAIM_TTL_SECS: u64 = 60 * 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnsureAction {
    Reuse,
    Derive,
    Provision,
    RequireInvite,
}

// The identity wire payload lives in cli-contract so the schema registry
// registers the real serialization type.
use heddle_cli_contract::cli::commands::wire::auth::IdentityOutput;

pub(crate) async fn cmd_identity(
    ctx: &(dyn CliContext + 'static),
    command: IdentityCommands,
) -> Result<()> {
    match command {
        IdentityCommands::Ensure { server, invite } => ensure(ctx, server, invite).await,
        IdentityCommands::ClaimLink { server, ttl_secs } => claim_link(ctx, server, ttl_secs).await,
        IdentityCommands::Serve { server } => identity_server::serve(server).await,
    }
}

async fn ensure(
    ctx: &(dyn CliContext + 'static),
    server: Option<String>,
    invite: Option<String>,
) -> Result<()> {
    let server = resolve_server(server.as_deref())?;
    let resolved = resolve_hosted_credential(Some(&server))?;
    let metadata = resolved
        .token
        .as_ref()
        .map(|token| headless_token_metadata(&token.id))
        .transpose()?;
    match ensure_action(
        metadata.as_ref().map(|value| value.is_derived),
        invite.is_some(),
        metadata
            .as_ref()
            .is_some_and(|value| is_local_agent_root(&value.subject, &value.proof_public_key_hex)),
    ) {
        EnsureAction::Reuse => {
            let metadata = metadata
                .ok_or_else(|| anyhow::anyhow!("identity decision lost credential metadata"))?;
            let metadata = refresh_expired_local_agent(&server, &metadata)?;
            print_identity(
                ctx,
                "identity_ensure",
                "reused",
                &server,
                &metadata.subject,
                None,
                None,
            )
        }
        EnsureAction::Derive => {
            cmd_auth_derive_agent(
                &server,
                None,
                DEFAULT_AGENT_TTL_SECS,
                Vec::new(),
                Vec::new(),
                None,
                None,
                true,
            )?;
            let store = config::credentials::load_credentials()?;
            let derived = store.servers.get(&server).ok_or_else(|| {
                anyhow::anyhow!("derived credential was not installed for {server}")
            })?;
            let metadata = headless_token_metadata(&derived.token)?;
            print_identity(
                ctx,
                "identity_ensure",
                "derived",
                &server,
                &metadata.subject,
                None,
                None,
            )
        }
        EnsureAction::Provision => {
            let invite = invite.ok_or_else(|| anyhow::anyhow!("identity decision lost invite"))?;
            create_on_behalf(ctx, server, invite).await
        }
        EnsureAction::RequireInvite => bail!(
            "no existing account credential for {server}; supply --invite to create a placeholder human account"
        ),
    }
}

pub(crate) fn ensure_action(
    derived: Option<bool>,
    has_invite: bool,
    local_agent_root: bool,
) -> EnsureAction {
    match derived {
        Some(true) => EnsureAction::Reuse,
        Some(false) if local_agent_root => EnsureAction::Reuse,
        Some(false) => EnsureAction::Derive,
        None if has_invite => EnsureAction::Provision,
        None => EnsureAction::RequireInvite,
    }
}

fn refresh_expired_local_agent(
    server: &str,
    metadata: &HeadlessTokenMetadata,
) -> Result<HeadlessTokenMetadata> {
    if !is_local_agent_root(&metadata.subject, &metadata.proof_public_key_hex) {
        return Ok(metadata.clone());
    }
    if !local_agent_credential_needs_refresh(metadata.expires_at.as_deref(), Utc::now()) {
        return Ok(metadata.clone());
    }
    let identity = agent_node_identity::load_or_create()?;
    let seed = identity.secret_key().to_bytes();
    let signer = Ed25519Signer::from_seed(&seed)
        .context("deriving the agent credential key from the persisted node identity")?;
    let node_id = identity.node_id().to_string();
    if hex::encode(signer.public_key()) != node_id
        || !metadata.proof_public_key_hex.eq_ignore_ascii_case(&node_id)
    {
        bail!("expired local agent root is not bound to this node's Iroh seed");
    }
    let root = mint_agent_root(&seed).context("reminting the expired local agent root")?;
    if root.public_key_hex() != node_id {
        bail!("reminted agent independent root is not bound to this node key");
    }
    let restricted = restrict_agent_account_root(&root.token, &signer, root.expires_at)
        .context("reapplying the local agent deny floor after remint")?;
    let refreshed =
        headless_token_metadata(&restricted).context("validating the reminted agent capability")?;
    if !refreshed
        .proof_public_key_hex
        .eq_ignore_ascii_case(&node_id)
        || refreshed.subject != root.subject
    {
        bail!("reminted agent capability is not bound to this node key");
    }
    config::credentials::store_server_credential(
        server,
        ServerCredential {
            token: restricted,
            subject: refreshed.subject.clone(),
            device_id: None,
            credential_id: None,
            private_key_pem: Some(root.private_key_pem),
            expires_at: Some(root.expires_at.to_rfc3339()),
        },
    )?;
    Ok(refreshed)
}

async fn create_on_behalf(
    ctx: &(dyn CliContext + 'static),
    server: String,
    invite: String,
) -> Result<()> {
    let identity = agent_node_identity::load_or_create()?;
    let seed = identity.secret_key().to_bytes();
    let signer = Ed25519Signer::from_seed(&seed)
        .context("deriving the agent credential key from the persisted node identity")?;
    let node_id = identity.node_id().to_string();
    if hex::encode(signer.public_key()) != node_id {
        bail!("persisted Iroh node key does not map to the agent credential key");
    }
    let root = mint_agent_root(&seed).context("minting the agent independent root locally")?;
    if root.public_key_hex() != node_id {
        bail!("agent independent root is not bound to this node key");
    }
    let user_config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &user_config,
        Some(server.clone()),
        HostedAuthMode::PresentedRoot {
            token: root.token.clone(),
            proof_key_pem: root.private_key_pem.clone(),
            subject: root.subject.clone(),
        },
    )?;
    let mut client = session.connect(([127, 0, 0, 1], 0).into()).await?;
    let operation_id = match ctx.operation_id_wire() {
        value if value.is_empty() => uuid::Uuid::new_v4().to_string(),
        value => value,
    };
    let response = client
        .create_agent_account(CreateAgentAccountRequest {
            invite_code: invite,
            agent_public_key: root.public_key.to_vec(),
            client_operation_id: operation_id,
        })
        .await
        .map_err(|error| {
            anyhow::anyhow!("creating placeholder account on behalf of human: {error}")
        });
    client.close().await;
    let response = response?;
    let restricted = restrict_agent_account_root(&root.token, &signer, root.expires_at)
        .context("applying the local agent deny floor and safe operation ceiling")?;
    let metadata = headless_token_metadata(&restricted)
        .context("validating the restricted client-minted agent capability")?;
    if !metadata.proof_public_key_hex.eq_ignore_ascii_case(&node_id)
        || metadata.subject != root.subject
    {
        bail!("client-minted agent capability is not bound to this node key");
    }
    config::credentials::store_server_credential(
        &server,
        ServerCredential {
            token: restricted,
            subject: metadata.subject.clone(),
            device_id: None,
            credential_id: None,
            private_key_pem: Some(root.private_key_pem),
            expires_at: Some(root.expires_at.to_rfc3339()),
        },
    )?;
    let owner_id = uuid::Uuid::parse_str(&response.account_id)
        .context("server returned a non-UUID account identity")?;
    let state = ClaimState::new(
        server.clone(),
        owner_id,
        metadata.subject.clone(),
        response.pet_name,
        node_id,
    );
    identity_state::store(&state)?;
    let (node_id, url) = reissue(&server, 900)?;
    identity_server::ensure_running(&server).await?;
    print_identity(
        ctx,
        "identity_ensure",
        "created_on_behalf",
        &server,
        &metadata.subject,
        Some(&node_id),
        Some(&url),
    )
}

async fn claim_link(
    ctx: &(dyn CliContext + 'static),
    server: Option<String>,
    ttl_secs: u64,
) -> Result<()> {
    let state = identity_state::load()?.ok_or_else(|| {
        anyhow::anyhow!("no agent-created placeholder account exists on this machine")
    })?;
    let server = server.unwrap_or_else(|| state.server.clone());
    if server != state.server {
        bail!("claim state belongs to {}, not {server}", state.server);
    }
    let subject = state.subject.clone();
    let (node_id, url) = reissue(&server, ttl_secs)?;
    identity_server::ensure_running(&server).await?;
    print_identity(
        ctx,
        "identity_claim_link",
        "claim_link_reissued",
        &server,
        &subject,
        Some(&node_id),
        Some(&url),
    )
}

fn reissue(server: &str, ttl_secs: u64) -> Result<(String, String)> {
    if ttl_secs == 0 || ttl_secs > MAX_CLAIM_TTL_SECS {
        bail!("--ttl must be between 1 and {MAX_CLAIM_TTL_SECS} seconds");
    }
    let mut state = identity_state::load()?
        .ok_or_else(|| anyhow::anyhow!("no claimable agent identity is stored"))?;
    if state.server != server {
        bail!("claim state belongs to {}, not {server}", state.server);
    }
    let origin = claim_web_origin(server)?;
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret).context("generating claim secret")?;
    let ttl_millis = i64::try_from(ttl_secs)?
        .checked_mul(1_000)
        .context("claim TTL overflow")?;
    let expires = chrono::Utc::now()
        .timestamp_millis()
        .checked_add(ttl_millis)
        .context("claim expiry overflow")?;
    if !state.reissue(&secret, expires) {
        bail!("account claim is already complete");
    }
    identity_state::store(&state)?;
    let encoded_secret = URL_SAFE_NO_PAD.encode(secret);
    let node_id = state.node_id;
    let url = claim_url(&origin, &node_id, &encoded_secret);
    Ok((node_id, url))
}

#[cfg(test)]
pub(crate) fn claim_link_url(server: &str, node_id: &str, encoded_secret: &str) -> Result<String> {
    Ok(claim_url(
        &claim_web_origin(server)?,
        node_id,
        encoded_secret,
    ))
}

/// Public web origin that may host `/claim/...` for `server`.
///
/// Default Heddle API hosts share `https://heddle.sh`. Any other HTTPS
/// authority is used as-is. Non-HTTPS or unparseable servers are refused so a
/// self-hosted claim secret is never placed on heddle.sh.
pub(crate) fn claim_web_origin(server: &str) -> Result<String> {
    let authority = canonical_server_authority(server).with_context(|| {
        format!("refusing to mint a claim URL for {server}: need an HTTPS server origin")
    })?;
    let url = reqwest::Url::parse(&authority).context("claim server origin is not a valid URL")?;
    let Some(host) = url.host_str() else {
        bail!("refusing to mint a claim URL for {server}: origin has no host");
    };
    if hosted_heddle_web_host(host) {
        return Ok("https://heddle.sh".to_string());
    }
    Ok(authority)
}

fn claim_url(origin: &str, node_id: &str, encoded_secret: &str) -> String {
    format!("{origin}/claim/hcl1.{node_id}.{encoded_secret}")
}

fn hosted_heddle_web_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "heddle.sh" || host.ends_with(".heddle.sh")
}

fn print_identity(
    ctx: &(dyn CliContext + 'static),
    output_kind: &str,
    outcome: &str,
    server: &str,
    subject: &str,
    node_id: Option<&str>,
    claim_url: Option<&str>,
) -> Result<()> {
    if ctx.should_output_json(None) {
        println!(
            "{}",
            serde_json::to_string(&IdentityOutput {
                output_kind: output_kind.to_string(),
                outcome: outcome.to_string(),
                server: server.to_string(),
                subject: subject.to_string(),
                node_id: node_id.map(str::to_string),
                claim_url: claim_url.map(str::to_string),
            })?
        );
    } else {
        println!("Identity: {outcome}");
        println!("Server: {server}");
        println!("Subject: {subject}");
        if let Some(node_id) = node_id {
            println!("NodeId: {node_id}");
        }
        if let Some(claim_url) = claim_url {
            println!("\nOpen this short-lived claim link:\n\n{claim_url}\n");
        }
    }
    Ok(())
}
