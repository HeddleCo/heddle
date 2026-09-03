//! Node-key remint and invite-create for `heddle auth login`.

use anyhow::{Context, Result, bail};
use api::heddle::api::v1alpha1::{
    CreateAgentAccountRequest, CreateAgentAccountResponse, SignedOwnerRoot,
};
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
    let subject = stored.subject.clone();
    let claimable_root = claimable_root_for_stored_account(server, &stored.private_key_pem)?;
    if let Some(root) = claimable_root {
        // weft#2041: pin over the FULL unrestricted root, not the proof-only
        // (bearer-less) session that weft rejects, and not the restricted token
        // stored below.
        pin_claimable_owner_root(
            server,
            &stored.full_root_token,
            &stored.private_key_pem,
            &subject,
            root,
        )
        .await?;
    }
    store_agent_root(
        server,
        stored.token,
        subject.clone(),
        stored.private_key_pem,
        stored.expires_at,
    )?;
    print_success(&subject);
    Ok(())
}

pub(crate) fn record_claimable_root_for_stored_account(
    server: &str,
    private_key_pem: &str,
) -> Result<()> {
    claimable_root_for_stored_account(server, private_key_pem).map(|_| ())
}

fn claimable_root_for_stored_account(
    server: &str,
    private_key_pem: &str,
) -> Result<Option<SignedOwnerRoot>> {
    let Some(mut state) = identity_state::load()? else {
        return Ok(None);
    };
    if !super::hosted::server_keys_match(&state.server, server) || state.is_claimed() {
        return Ok(None);
    }
    let signer = Ed25519Signer::from_pem(private_key_pem)
        .context("loading the agent proof key for the claimable owner root")?;
    let root = super::owner_root::mint_and_record_claimable_root(
        &mut state,
        &signer,
        chrono::Utc::now().timestamp(),
    )?;
    identity_state::store(&state)?;
    Ok(Some(root))
}

#[cfg(test)]
async fn upload_reminted_owner_root(
    client: &mut super::HostedClient,
    private_key_pem: &str,
    root: SignedOwnerRoot,
) -> Result<()> {
    let signer = Ed25519Signer::from_pem(private_key_pem)
        .context("loading the agent proof key for BootstrapOwnerRoot")?;
    super::owner_root::upload_claimable_root(client, &signer, root).await
}

#[cfg(test)]
pub(crate) async fn remint_with_client_for_test(
    server: &str,
    client: &mut super::HostedClient,
) -> Result<()> {
    let stored = mint_restricted_agent_root()?;
    if let Some(root) = claimable_root_for_stored_account(server, &stored.private_key_pem)? {
        upload_reminted_owner_root(client, &stored.private_key_pem, root).await?;
    }
    store_agent_root(
        server,
        stored.token,
        stored.subject,
        stored.private_key_pem,
        stored.expires_at,
    )
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
    let mut client = session.connect(server).await?;
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
    // The proof-only connection consumed the invite. `BootstrapOwnerRoot`
    // requires an agent BEARER (weft#2041), so close this bearer-less session
    // and re-open one presenting the full unrestricted root token.
    client.close().await;
    let full_root_token = minted.full_root_token.clone();
    let proof_key_pem = minted.private_key_pem.clone();
    let subject = minted.subject.clone();
    let output = finish_invite_create(server, minted, response)?;
    if let Some(state) = identity_state::load()?
        && let Some(root) = super::owner_root::load_recorded_root(&state)?
    {
        pin_claimable_owner_root(server, &full_root_token, &proof_key_pem, &subject, root).await?;
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
    /// The restricted account-root capability persisted to the on-disk
    /// credential. This is what everyday hosted calls (and `derive-agent`)
    /// use as the parent bearer.
    token: String,
    /// The FULL, unrestricted client-minted root token, held only in memory
    /// during the login flow. `BootstrapOwnerRoot` is presented this bearer
    /// (weft#2041): the server requires an agent capability for the owner-root
    /// pin, and pinning over the full root means the pin never depends on the
    /// stored credential's narrowed authority.
    full_root_token: String,
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
        .context("applying the local agent account-root deny floor")?;
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
        full_root_token: root.token,
        subject: metadata.subject,
        public_key: root.public_key,
        private_key_pem: root.private_key_pem,
        expires_at: root.expires_at,
    })
}

/// Early-pin the recorded claimable owner root, presenting the FULL,
/// unrestricted client-minted root token as the request bearer (weft#2041).
///
/// `BootstrapOwnerRoot` requires an agent bearer capability and is deliberately
/// OFF the independent-root method floor so it accepts a client-minted agent
/// root. The invite/remint flow previously issued this call over a proof-only
/// session that carried no bearer, so weft rejected it with
/// `invalid bearer capability`. We open a dedicated session that presents the
/// unrestricted root held in memory during login, rather than the restricted
/// credential that is persisted to disk.
async fn pin_claimable_owner_root(
    server: &str,
    full_root_token: &str,
    proof_key_pem: &str,
    subject: &str,
    root: SignedOwnerRoot,
) -> Result<()> {
    let user_config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &user_config,
        Some(server.to_string()),
        owner_root_pin_auth_mode(full_root_token, proof_key_pem, subject),
    )?;
    let mut client = session.connect(server).await?;
    let signer = Ed25519Signer::from_pem(proof_key_pem)
        .context("loading the agent proof key for BootstrapOwnerRoot")?;
    let result = super::owner_root::upload_claimable_root(&mut client, &signer, root).await;
    client.close().await;
    result
}

/// Build the auth mode for the owner-root pin: present the full unrestricted
/// root token as the bearer, proven by the agent's own device key. Extracted so
/// the token selection is unit-testable without a live endpoint.
fn owner_root_pin_auth_mode(
    full_root_token: &str,
    proof_key_pem: &str,
    subject: &str,
) -> HostedAuthMode {
    HostedAuthMode::PresentedRoot {
        token: full_root_token.to_string(),
        proof_key_pem: proof_key_pem.to_string(),
        subject: subject.to_string(),
    }
}

/// The tokens involved in an owner-root pin, surfaced for tests: the restricted
/// credential persisted to disk vs. the unrestricted bearer the pin presents.
#[cfg(test)]
pub(crate) struct OwnerRootPinProbe {
    pub stored_token: String,
    pub presented_bearer: String,
    pub full_root_token: String,
    pub subject: String,
}

/// Mint an agent root exactly as the invite/remint flow does, then resolve the
/// bearer the owner-root pin would present. Lets tests assert weft#2041's
/// invariant (pin over the full unrestricted root, store the restricted one)
/// without a live endpoint.
#[cfg(test)]
pub(crate) fn owner_root_pin_probe() -> Result<OwnerRootPinProbe> {
    let minted = mint_restricted_agent_root()?;
    let presented_bearer = match owner_root_pin_auth_mode(
        &minted.full_root_token,
        &minted.private_key_pem,
        &minted.subject,
    ) {
        HostedAuthMode::PresentedRoot { token, .. } => token,
        _ => bail!("owner-root pin must present a root bearer"),
    };
    Ok(OwnerRootPinProbe {
        stored_token: minted.token,
        presented_bearer,
        full_root_token: minted.full_root_token,
        subject: minted.subject,
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        net::Ipv4Addr,
        sync::{Arc, Mutex},
    };

    use api::{
        framing::{decode_request_prelude, encode_success_response},
        heddle::api::v1alpha1::HostedSpool,
    };
    use bytes::Bytes;
    use crypto::Ed25519Signer;
    use iroh::{Endpoint, RelayMode, endpoint::presets};
    use prost::Message;
    use tokio::task::JoinHandle;

    use crate::hosted_runtime::hosted::{CallContextFactory, HostedClient};

    const CREATE_SPOOL: &str = "/heddle.api.v1alpha1.RegistryService/CreateSpool";

    pub(crate) async fn start_recording_client() -> (
        HostedClient,
        JoinHandle<()>,
        Arc<Mutex<Vec<String>>>,
        String,
    ) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let server_calls = Arc::clone(&calls);
        let server = Endpoint::builder(presets::Minimal)
            .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("test server address")
            .bind()
            .await
            .expect("test server endpoint");
        let server_addr = server.addr();
        let server_task = tokio::spawn(async move {
            let connection = server
                .accept()
                .await
                .expect("hosted test connection")
                .await
                .expect("connect hosted test client");
            while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                let mut request = Vec::new();
                let method = loop {
                    let chunk = recv
                        .read_chunk(api::framing::MAX_CONTROL_BODY + 6)
                        .await
                        .expect("read request")
                        .expect("request prelude");
                    request.extend_from_slice(&chunk);
                    if let Some((prelude, _)) =
                        decode_request_prelude(&request).expect("decode request prelude")
                    {
                        break prelude.method.to_string();
                    }
                };
                server_calls
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push(method.clone());
                while recv
                    .read_chunk(api::framing::MAX_CONTROL_BODY + 6)
                    .await
                    .is_ok_and(|chunk| chunk.is_some())
                {}
                let body = if method == CREATE_SPOOL {
                    HostedSpool::default().encode_to_vec()
                } else {
                    Vec::new()
                };
                send.write_chunk(Bytes::from(
                    encode_success_response(&body).expect("encode response"),
                ))
                .await
                .expect("write response");
                send.finish().expect("finish response");
            }
            server.close().await;
        });
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("test client address")
            .bind()
            .await
            .expect("test client endpoint");
        let signer = Ed25519Signer::generate().expect("test signer");
        let signer_pem = signer.to_pem().expect("test signer pem");
        let context = CallContextFactory::default()
            .with_signing_key_pem(&signer_pem, "principal:test")
            .expect("test call context");
        let client = HostedClient::connect_addr_with_context(endpoint, server_addr, context)
            .await
            .expect("connect test client");
        (client, server_task, calls, signer_pem)
    }
}
