//! Node-key remint and invite-create for `heddle auth login`.

use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1::{
    self as v2, ProvisionAccountRequest, ProvisionAccountResponse, SignedOwnerRoot,
};
use config::UserConfig;
use crypto::{Ed25519Signer, Signer as _};
use prost::Message;

use super::{
    HostedAuthMode, HostedSession, agent_node_identity,
    auth::{AgentAccountCreated, AuthLoginOutcome, headless_token_metadata},
    auth_requests::AuthOptions,
    device_flow::restrict_agent_account_root,
    identity_state::{self, ClaimState},
    root_mint::{is_local_agent_root, mint_agent_root},
};

pub(crate) async fn remint(server: &str) -> Result<AuthLoginOutcome> {
    let minted = mint_agent_credential_for_server(server)?;
    if let Some(root) = claimable_root_for_stored_account(server, &minted.private_key_pem)? {
        pin_claimable_owner_root(
            server,
            &minted.token,
            &minted.private_key_pem,
            &minted.subject,
            root,
        )
        .await?;
    }
    let subject = minted.subject.clone();
    store_agent_credential(server, minted)?;
    Ok(AuthLoginOutcome::Authenticated {
        subject,
        credential_saved: true,
    })
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
    if !super::hosted::server_keys_match(&state.server, server) || state.consent_issued() {
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
        .context("loading the agent proof key for BootstrapOwnership")?;
    super::owner_root::upload_claimable_root(client, &signer, root).await
}

#[cfg(test)]
pub(crate) async fn remint_with_client_for_test(
    server: &str,
    client: &mut super::HostedClient,
) -> Result<()> {
    let stored = mint_agent_credential_for_server(server)?;
    if let Some(root) = claimable_root_for_stored_account(server, &stored.private_key_pem)? {
        upload_reminted_owner_root(client, &stored.private_key_pem, root).await?;
    }
    store_agent_credential(server, stored)
}

pub(crate) async fn create_with_invite(
    options: &AuthOptions,
    server: &str,
    invite: String,
) -> Result<AuthLoginOutcome> {
    let operation_id = options
        .operation_id()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    provision(server, invite.into_bytes(), operation_id).await
}

async fn provision(
    server: &str,
    invitation_secret: Vec<u8>,
    operation_id: String,
) -> Result<AuthLoginOutcome> {
    let minted = mint_agent_credential()?;
    let user_config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &user_config,
        Some(server.to_string()),
        HostedAuthMode::ProofOnly {
            proof_key_pem: minted.private_key_pem.clone(),
            signing_identity: format!("principal:{}", minted.subject),
        },
    )?;
    let client = session.connect(server).await?;
    let response = client
        .native()
        .await?
        .api
        .call::<thread_api::rpc::IdentityServiceProvisionAccount>(&ProvisionAccountRequest {
            invitation_secret,
            agent_public_key: minted.public_key.to_vec(),
            client_operation_id: operation_id.clone(),
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
    client.close().await;
    if !response.receipt.as_ref().is_some_and(|receipt| {
        receipt.client_operation_id == operation_id
            && matches!(
                receipt.outcome,
                Some(v2::mutation_receipt::Outcome::Applied(_))
            )
    }) {
        bail!("account provisioning did not apply the requested operation");
    }
    let output = finish_invite_create(server, minted, response)?;
    if matches!(&output, AuthLoginOutcome::AgentAccountCreated(_))
        && let Some(state) = identity_state::load()?
        && let Some(root) = super::owner_root::load_recorded_root(&state)?
    {
        let credential = config::credentials::get_server_credential(server)?
            .context("provisioned agent credential missing")?;
        pin_claimable_owner_root(
            server,
            &credential.token,
            credential
                .private_key_pem
                .as_deref()
                .context("provisioned agent signer missing")?,
            &credential.subject,
            root,
        )
        .await?;
    }
    Ok(output)
}

fn finish_invite_create(
    server: &str,
    mut minted: AgentRoot,
    response: ProvisionAccountResponse,
) -> Result<AuthLoginOutcome> {
    let principal = response
        .principal
        .context("provisioning response has no principal")?;
    let owner_id = uuid::Uuid::parse_str(&principal.account_id)
        .context("server returned a non-UUID account identity")?;
    if owner_id.is_nil() || principal.id != principal.account_id {
        bail!("provisioning principal does not identify its account");
    }
    let tier = v2::RootingTier::try_from(principal.rooting_tier)
        .ok()
        .filter(|tier| *tier != v2::RootingTier::Unspecified)
        .context("provisioning response has no account tier")?;
    bind_registered_agent(
        &mut minted,
        response
            .credential
            .context("provisioning credential missing")?,
    )?;
    let web_origin = match response.claim_web_origin.trim() {
        "" => None,
        value => Some(value.to_string()),
    };
    let subject = minted.subject.clone();
    if let Some(ownership) = response.ownership.as_ref() {
        verify_provisioned_owner(ownership, owner_id)?;
    }
    if tier != v2::RootingTier::AgentRooted {
        store_agent_credential(server, minted)?;
        return Ok(AuthLoginOutcome::Authenticated {
            subject,
            credential_saved: true,
        });
    }
    let account_id = principal.account_id;
    let pet_name = principal.display_name;
    let node_id = hex::encode(minted.public_key);
    let mut claim_state = identity_state::load()?
        .filter(|state| {
            super::hosted::server_keys_match(&state.server, server)
                && state.owner_id == owner_id
                && state.node_id == node_id
        })
        .unwrap_or_else(|| {
            ClaimState::new(
                server.to_string(),
                owner_id,
                subject.clone(),
                pet_name.clone(),
                node_id,
                web_origin,
            )
        });
    let signer = Ed25519Signer::from_pem(&minted.private_key_pem)
        .context("loading the agent proof key for the claimable owner root")?;
    if let Some(ownership) = response.ownership {
        let signed = ownership.root.context("provisioned owner root missing")?;
        if !ownership.accepted_transitions.is_empty()
            || !signed
                .root
                .as_ref()
                .is_some_and(|root| root.claimable_deferred_human)
            || repo::seq0_authority_public_key(&signed)? != signer.public_key()
        {
            bail!("unclaimed account ownership is not the proved agent's claimable root");
        }
        if let Some(previous) = super::owner_root::load_recorded_root(&claim_state)?
            && previous != signed
        {
            bail!("provisioned owner root differs from the locally recorded original");
        }
        use prost::Message;
        claim_state.record_claimable_owner_root(signer.public_key(), &signed.encode_to_vec());
    } else {
        super::owner_root::mint_and_record_claimable_root(
            &mut claim_state,
            &signer,
            chrono::Utc::now().timestamp(),
        )?;
    }
    identity_state::store(&claim_state)?;
    store_agent_credential(server, minted)?;
    Ok(AuthLoginOutcome::AgentAccountCreated(AgentAccountCreated {
        account_id: account_id.clone(),
        pet_name,
        subject,
        authenticated: true,
        credential_saved: true,
        next: super::auth::HumanPromotionDirective {
            kind: "human_promotion_required",
            summary: "Account is active and usable now; a human must complete the claim ceremony to bind ownership.",
            account_id,
            command: "heddle claim",
            promotion_uri: None,
        },
    }))
}

fn verify_provisioned_owner(observed: &v2::OwnerState, account: uuid::Uuid) -> Result<()> {
    let signed = observed
        .root
        .as_ref()
        .context("provisioned owner root missing")?;
    if observed
        .owner
        .as_ref()
        .is_none_or(|owner| owner.id != account.to_string())
        || signed
            .root
            .as_ref()
            .is_none_or(|root| root.account_uuid != account.as_bytes())
        || observed.resource_keyring.is_some()
    {
        bail!("provisioned ownership does not bind the registered account");
    }
    repo::verify_account_owner_observation(observed, chrono::Utc::now().timestamp())?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn provision_response_for_test(
    account: &str,
    name: &str,
    origin: &str,
) -> ProvisionAccountResponse {
    let identity = agent_node_identity::load_or_create().expect("fixture node");
    let key = identity.node_id().to_string();
    ProvisionAccountResponse {
        principal: Some(v2::PrincipalRecord {
            id: account.into(),
            account_id: account.into(),
            display_name: name.into(),
            rooting_tier: v2::RootingTier::AgentRooted as i32,
            ..Default::default()
        }),
        credential: Some(v2::CredentialResult {
            mint_root_attachment: None,
            outcome: Some(v2::credential_result::Outcome::ClientOwned(
                v2::ClientOwnedCredential {
                    r#ref: Some(v2::RecordRef {
                        id: "fixture-agent-credential".into(),
                        spool: None,
                    }),
                    subject: format!("agent-key:{key}"),
                    proof_public_key: hex::decode(&key).expect("key"),
                    kind: v2::CredentialKind::Agent as i32,
                },
            )),
            session: Some(v2::SessionRecord {
                r#ref: Some(v2::RecordRef {
                    id: "fixture-agent-session".into(),
                    spool: None,
                }),
                expires_at: Some(prost_types::Timestamp {
                    seconds: (chrono::Utc::now() + chrono::Duration::days(30)).timestamp(),
                    nanos: 0,
                }),
                ..Default::default()
            }),
            owner_authorization: None,
        }),
        claim_web_origin: origin.into(),
        ..Default::default()
    }
}

#[cfg(test)]
pub(crate) fn finish_invite_create_from_response(
    server: &str,
    response: ProvisionAccountResponse,
) -> Result<AgentAccountCreated> {
    match finish_invite_create(server, mint_agent_credential()?, response)? {
        AuthLoginOutcome::AgentAccountCreated(created) => Ok(created),
        _ => bail!("fixture expected unclaimed account"),
    }
}

fn bind_registered_agent(minted: &mut AgentRoot, result: v2::CredentialResult) -> Result<()> {
    let Some(v2::credential_result::Outcome::ClientOwned(registered)) = result.outcome else {
        bail!("keyed agent provisioning must register the client's own credential");
    };
    if registered.kind != v2::CredentialKind::Agent as i32
        || registered.proof_public_key != minted.public_key
        || registered.subject != minted.subject
        || result.owner_authorization.is_some()
    {
        bail!("provisioning credential differs from the proved agent key or authority class");
    }
    let reference = registered
        .r#ref
        .context("registered credential reference missing")?;
    let session = result.session.context("registered session missing")?;
    let session_ref = session
        .r#ref
        .context("registered session reference missing")?;
    if reference.spool.is_some()
        || reference.id.is_empty()
        || session_ref.spool.is_some()
        || session_ref.id.is_empty()
        || session.revoked
    {
        bail!("registered agent credential or session is not active");
    }
    let expiry = session
        .expires_at
        .context("registered session expiry missing")?;
    let expires_at = chrono::DateTime::from_timestamp(expiry.seconds, expiry.nanos.try_into()?)
        .context("registered session expiry invalid")?
        .min(minted.expires_at);
    let signer = Ed25519Signer::from_pem(&minted.private_key_pem)?;
    let root = super::root_mint::mint_independent_root(super::root_mint::IndependentRootMint {
        seed: &signer.to_seed(),
        subject: &minted.subject,
        ttl: super::root_mint::ACCOUNT_ROOT_TTL,
        credential_id: Some(&reference.id),
        session_id: Some(&session_ref.id),
        expires_at: Some(expires_at),
    })?;
    minted.token = restrict_agent_account_root(&root.token, &signer, root.expires_at)?;
    minted.mint_root_attachment = result
        .mint_root_attachment
        .map(|proof| proof.encode_to_vec());
    minted.credential_id = Some(reference.id);
    minted.expires_at = root.expires_at;
    Ok(())
}

struct AgentRoot {
    mint_root_attachment: Option<Vec<u8>>,
    credential_id: Option<String>,
    /// The account-root capability with local agent attribution persisted to the on-disk
    /// credential. This is what everyday hosted calls (and `derive-agent`)
    /// use as the parent bearer.
    token: String,
    subject: String,
    public_key: [u8; 32],
    private_key_pem: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

fn store_agent_credential(server: &str, minted: AgentRoot) -> Result<()> {
    config::credentials::store_server_credential(
        server,
        config::credentials::ServerCredential {
            mint_root_attachment: minted.mint_root_attachment,
            token: minted.token,
            subject: minted.subject,
            device_id: None,
            credential_id: minted.credential_id,
            private_key_pem: Some(minted.private_key_pem),
            expires_at: Some(minted.expires_at.to_rfc3339()),
        },
    )
}

fn mint_agent_credential_for_server(server: &str) -> Result<AgentRoot> {
    let mut minted = mint_agent_credential()?;
    if let Some(stored) = config::credentials::get_server_credential(server)?
        && stored.subject == minted.subject
    {
        let metadata = headless_token_metadata(&stored.token)?;
        if metadata.proof_public_key_hex != hex::encode(minted.public_key) {
            bail!("stored agent credential is attached to another key");
        }
        let session = super::root_mint::authority_session_fact(&stored.token)?;
        let root = super::root_mint::remint_stored_root(
            &minted.private_key_pem,
            &minted.subject,
            stored.credential_id.as_deref(),
            Some(&session),
        )?;
        let signer = Ed25519Signer::from_pem(&minted.private_key_pem)?;
        minted.token = restrict_agent_account_root(&root.token, &signer, root.expires_at)?;
        minted.mint_root_attachment = stored.mint_root_attachment;
        minted.credential_id = root.credential_id;
        minted.expires_at = root.expires_at;
    }
    Ok(minted)
}

fn mint_agent_credential() -> Result<AgentRoot> {
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
        .context("binding local agent attribution to the account credential")?;
    let metadata = headless_token_metadata(&restricted)
        .context("validating the restricted client-minted agent capability")?;
    if !metadata.proof_public_key_hex.eq_ignore_ascii_case(&node_id)
        || metadata.subject != root.subject
        || !is_local_agent_root(&metadata.subject, &metadata.proof_public_key_hex)
    {
        bail!("client-minted agent capability is not bound to this node key");
    }
    Ok(AgentRoot {
        mint_root_attachment: None,
        credential_id: None,
        token: restricted,
        subject: metadata.subject,
        public_key: root.public_key,
        private_key_pem: root.private_key_pem,
        expires_at: root.expires_at,
    })
}

/// Install the recorded owner root using the registered, key-bound agent credential.
async fn pin_claimable_owner_root(
    server: &str,
    token: &str,
    proof_key_pem: &str,
    subject: &str,
    root: SignedOwnerRoot,
) -> Result<()> {
    let session = owner_root_pin_session(server, token, proof_key_pem, subject)?;
    let client = session.connect(server).await?;
    finish_owner_root_pin(client, proof_key_pem, root).await
}

/// Confirm the claim root using the stored registered session. The outbound
/// endpoint remains separate from the device endpoint serving the claim link.
pub(crate) async fn ensure_owner_root_pinned(server: &str) -> Result<()> {
    let credential = config::credentials::get_server_credential(server)?
        .context("claim requires the registered agent credential")?;
    let key = credential
        .private_key_pem
        .as_deref()
        .context("agent signer missing")?;
    let Some(root) = claimable_root_for_stored_account(server, key)? else {
        return Ok(());
    };
    let session = owner_root_pin_session(server, &credential.token, key, &credential.subject)?;
    let client = session.connect_outbound(server).await?;
    finish_owner_root_pin(client, key, root).await
}

fn owner_root_pin_session(
    server: &str,
    token: &str,
    proof_key_pem: &str,
    subject: &str,
) -> Result<HostedSession> {
    let user_config = UserConfig::load_default()?;
    HostedSession::build(
        &user_config,
        Some(server.to_string()),
        owner_root_pin_auth_mode(token, proof_key_pem, subject),
    )
}

async fn finish_owner_root_pin(
    mut client: super::HostedClient,
    proof_key_pem: &str,
    root: SignedOwnerRoot,
) -> Result<()> {
    let signer = Ed25519Signer::from_pem(proof_key_pem)
        .context("loading the agent proof key for BootstrapOwnership")?;
    let result = super::owner_root::upload_claimable_root(&mut client, &signer, root).await;
    client.close().await;
    result
}

/// Preserve the registered bearer and its proof key for owner bootstrap.
fn owner_root_pin_auth_mode(token: &str, proof_key_pem: &str, subject: &str) -> HostedAuthMode {
    HostedAuthMode::PresentedRoot {
        token: token.to_string(),
        proof_key_pem: proof_key_pem.to_string(),
        subject: subject.to_string(),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        net::Ipv4Addr,
        sync::{Arc, Mutex},
    };

    use api::{
        framing::{decode_request_frame, encode_success_response},
        heddle::api::v2alpha1::*,
    };
    use bytes::Bytes;
    use crypto::Ed25519Signer;
    use iroh::{Endpoint, RelayMode, endpoint::presets};
    use prost::Message;
    use tokio::task::JoinHandle;

    use crate::hosted_runtime::hosted::{CallContextFactory, HostedClient};

    const DESCRIBE: &str = "/heddle.api.v2alpha1.EndpointService/DescribeEndpoint";
    const BOOTSTRAP: &str = "/heddle.api.v2alpha1.OwnerAuthorizationService/BootstrapOwnership";

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
        let server_key = server_addr.id.as_bytes().to_vec();
        let server_task = tokio::spawn(async move {
            let connection = server
                .accept()
                .await
                .expect("hosted test connection")
                .await
                .expect("connect hosted test client");
            while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                let mut request = Vec::new();
                while let Some(chunk) = recv
                    .read_chunk(api::framing::MAX_CONTROL_BODY + 6)
                    .await
                    .expect("read frame")
                {
                    request.extend_from_slice(&chunk);
                }
                let frame = decode_request_frame(&request).expect("native request");
                let method = frame.method;
                server_calls
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push(method.to_owned());
                let body = match method {
                    DESCRIBE => DescribeEndpointResponse {
                        endpoint: Some(EndpointRef {
                            public_key: server_key.clone(),
                            kind: EndpointKind::Weft as i32,
                        }),
                        supported_packages: vec!["heddle.api.v2alpha1".into()],
                        implemented_methods: vec![DESCRIBE.into(), BOOTSTRAP.into()],
                        ..Default::default()
                    }
                    .encode_to_vec(),
                    BOOTSTRAP => {
                        let body = BootstrapOwnershipRequest::decode(frame.body)
                            .expect("native bootstrap");
                        let root = body.root.as_ref().expect("root");
                        let key: [u8; 32] = repo::seq0_authority_public_key(root)
                            .expect("root key")
                            .try_into()
                            .expect("key");
                        thread_api::request_proof::verify(
                            &frame.context,
                            api::v2::method_descriptor(BOOTSTRAP).expect("method"),
                            frame.body,
                            &key,
                            chrono::Utc::now().timestamp_millis(),
                        )
                        .expect("exact bootstrap proof");
                        let initial =
                            heddleco_capability_verifier::verify_owner_root(root).expect("root");
                        heddleco_capability_verifier::verify_owner_key_binding(
                            body.binding.as_ref().expect("binding"),
                            &initial,
                            &root
                                .root
                                .as_ref()
                                .expect("body")
                                .account_uuid
                                .as_slice()
                                .try_into()
                                .expect("UUID"),
                        )
                        .expect("root binding");
                        MutationResponse {
                            receipt: Some(MutationReceipt {
                                client_operation_id: body.client_operation_id,
                                outcome: Some(mutation_receipt::Outcome::Applied(
                                    Applied::default(),
                                )),
                                ..Default::default()
                            }),
                        }
                        .encode_to_vec()
                    }
                    _ => panic!("unexpected native route {method}"),
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
        let identity = super::agent_node_identity::load_or_create().expect("fixture identity");
        let signer =
            Ed25519Signer::from_seed(&identity.secret_key().to_bytes()).expect("test signer");
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
