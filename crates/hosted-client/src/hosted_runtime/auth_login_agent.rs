//! Node-key remint and invite-create for `heddle auth login`.

use anyhow::{Context, Result, bail};
use api::heddle::api::v1alpha1::{CreateAgentAccountRequest, CreateAgentAccountResponse};
use config::UserConfig;
use crypto::{Ed25519Signer, Signer as _};
use heddle_cli_args::CliContext;
use heddle_cli_contract::cli::commands::wire::auth::{
    AgentAccountCreatedOutput, HumanPromotionDirective,
};

use super::{
    HostedAuthMode, HostedSession, agent_node_identity,
    auth::headless_token_metadata,
    auth_login::{print_success, store_agent_root},
    device_flow::restrict_agent_account_root,
    identity_state::{self, ClaimState},
    root_mint::{is_local_agent_root, mint_agent_root},
};

pub(crate) async fn remint(server: &str) -> Result<()> {
    let stored = mint_restricted_agent_root()?;
    record_claimable_root_for_stored_account(server, &stored.private_key_pem)?;
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

pub(crate) fn record_claimable_root_for_stored_account(
    server: &str,
    private_key_pem: &str,
) -> Result<()> {
    let Some(mut state) = identity_state::load()? else {
        return Ok(());
    };
    if !super::hosted::server_keys_match(&state.server, server) {
        return Ok(());
    }
    let signer = Ed25519Signer::from_pem(private_key_pem)
        .context("loading the agent proof key for the claimable owner root")?;
    super::owner_root::mint_and_record_claimable_root(
        &mut state,
        &signer,
        chrono::Utc::now().timestamp(),
    )?;
    identity_state::store(&state)
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
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            client.close().await;
            return Err(error);
        }
    };
    let output = finish_invite_create(server, minted, response)?;
    if let Some(state) = identity_state::load()?
        && let Some(root) = super::owner_root::load_recorded_root(&state)?
    {
        let upload = super::owner_root::upload_claimable_root(&mut client, root).await;
        client.close().await;
        upload?;
    } else {
        client.close().await;
    }
    emit_created(ctx, &output)?;
    Ok(())
}

fn emit_created(ctx: &dyn CliContext, output: &AgentAccountCreatedOutput) -> Result<()> {
    if ctx.should_output_json(None) {
        println!("{}", serde_json::to_string(output)?);
    } else {
        print_success(&output.subject);
        println!(
            "Agent account {} is active; a human can claim it later.",
            output.pet_name
        );
        println!("Next: {}", output.next.command);
    }
    Ok(())
}

fn finish_invite_create(
    server: &str,
    minted: RestrictedAgentRoot,
    response: CreateAgentAccountResponse,
) -> Result<AgentAccountCreatedOutput> {
    let owner_id = uuid::Uuid::parse_str(&response.account_id)
        .context("server returned a non-UUID account identity")?;
    let web_origin = match response.web_origin.trim() {
        "" => None,
        value => Some(value.to_string()),
    };
    let subject = minted.subject.clone();
    let private_key_pem = minted.private_key_pem.clone();
    store_agent_root(
        server,
        minted.token,
        minted.subject.clone(),
        minted.private_key_pem,
        minted.expires_at,
    )?;
    let account_id = response.account_id;
    let pet_name = response.pet_name;
    // Stored for later routing. `heddle claim` binds this to the configured
    // hosted server before minting the local claim bearer.
    let mut claim_state = ClaimState::new(
        server.to_string(),
        owner_id,
        subject.clone(),
        pet_name.clone(),
        hex::encode(minted.public_key),
        web_origin,
    );
    let signer = Ed25519Signer::from_pem(&private_key_pem)
        .context("loading the agent proof key for the claimable owner root")?;
    super::owner_root::mint_and_record_claimable_root(
        &mut claim_state,
        &signer,
        chrono::Utc::now().timestamp(),
    )?;
    identity_state::store(&claim_state)?;
    Ok(AgentAccountCreatedOutput {
        output_kind: "agent_account_created",
        account_id: account_id.clone(),
        pet_name,
        subject,
        authenticated: true,
        credential_saved: true,
        next: HumanPromotionDirective {
            kind: "human_promotion_required",
            summary: "Account is active and usable now; a human must complete the claim ceremony to bind ownership.",
            account_id,
            command: "heddle claim",
            promotion_uri: None,
        },
    })
}

#[cfg(test)]
pub(crate) fn finish_invite_create_from_response(
    server: &str,
    response: CreateAgentAccountResponse,
) -> Result<AgentAccountCreatedOutput> {
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
