//! Node-key remint and invite-create for `heddle auth login`.

use anyhow::{Context, Result, bail};
use api::heddle::api::v1alpha1::{CreateAgentAccountRequest, CreateAgentAccountResponse};
use config::UserConfig;
use crypto::{Ed25519Signer, Signer as _};
use heddle_cli_args::CliContext;

use super::{
    HostedAuthMode, HostedSession, agent_node_identity,
    auth::headless_token_metadata,
    auth_login::{print_claim_link, print_success, store_agent_root},
    device_flow::restrict_agent_account_root,
    identity_state::{self, ClaimState},
    root_mint::{is_local_agent_root, mint_agent_root},
};

pub(crate) async fn remint(server: &str) -> Result<()> {
    let stored = mint_restricted_agent_root()?;
    store_agent_root(
        server,
        stored.token,
        stored.subject.clone(),
        stored.private_key_pem,
        stored.expires_at,
    )?;
    print_success(&stored.subject);
    Ok(())
}

pub(crate) async fn create_with_invite(
    ctx: &dyn CliContext,
    server: &str,
    invite: String,
) -> Result<()> {
    let minted = mint_restricted_agent_root()?;
    let user_config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &user_config,
        Some(server.to_string()),
        HostedAuthMode::ProofOnly {
            proof_key_pem: minted.private_key_pem.clone(),
            signing_identity: format!("principal:{}", minted.subject),
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
            agent_public_key: minted.public_key.to_vec(),
            client_operation_id: operation_id,
        })
        .await
        .map_err(|error| {
            anyhow::anyhow!("creating placeholder account on behalf of human: {error}")
        });
    client.close().await;
    let response = response?;
    let subject = minted.subject.clone();
    let claim_link = finish_invite_create(server, minted, response)?;
    print_success(&subject);
    print_claim_link(&claim_link);
    Ok(())
}

pub(crate) fn invite_created_claim_link(claim_token: &str) -> Result<&str> {
    match claim_token.trim() {
        "" => bail!("CreateAgentAccount returned no claim token; the human has no claim link"),
        token => Ok(token),
    }
}

fn finish_invite_create(
    server: &str,
    minted: RestrictedAgentRoot,
    response: CreateAgentAccountResponse,
) -> Result<String> {
    let owner_id = uuid::Uuid::parse_str(&response.account_id)
        .context("server returned a non-UUID account identity")?;
    let claim_link = invite_created_claim_link(&response.claim_token).map(str::to_string);
    store_agent_root(
        server,
        minted.token,
        minted.subject.clone(),
        minted.private_key_pem,
        minted.expires_at,
    )?;
    identity_state::store(&ClaimState::new(
        server.to_string(),
        owner_id,
        minted.subject,
        response.pet_name,
        hex::encode(minted.public_key),
    ))?;
    claim_link
}

#[cfg(test)]
pub(crate) fn finish_invite_create_from_response(
    server: &str,
    response: CreateAgentAccountResponse,
) -> Result<String> {
    finish_invite_create(server, mint_restricted_agent_root()?, response)
}

struct RestrictedAgentRoot {
    token: String,
    subject: String,
    public_key: [u8; 32],
    private_key_pem: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

fn mint_restricted_agent_root() -> Result<RestrictedAgentRoot> {
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
    let restricted = restrict_agent_account_root(&root.token, &signer, root.expires_at)
        .context("applying the local agent deny floor and safe operation ceiling")?;
    let metadata = headless_token_metadata(&restricted)
        .context("validating the restricted client-minted agent capability")?;
    if !metadata.proof_public_key_hex.eq_ignore_ascii_case(&node_id)
        || metadata.subject != root.subject
        || !is_local_agent_root(&metadata.subject, &metadata.proof_public_key_hex)
    {
        bail!("client-minted agent capability is not bound to this node key");
    }
    Ok(RestrictedAgentRoot {
        token: restricted,
        subject: metadata.subject,
        public_key: root.public_key,
        private_key_pem: root.private_key_pem,
        expires_at: root.expires_at,
    })
}
