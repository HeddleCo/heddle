//! Foreground native account-claim issuer. Browser registration stays browser-signed.
use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1::*;
use crypto::Signer as _;
use prost::Message;

use super::{
    auth::headless_token_metadata,
    device_flow::{AgentAttenuation, attenuate_for_agent},
    hosted::{
        HostedClient,
        claim_protocol::{CLAIM_PREPARE_METHOD, CLAIM_SIGN_METHOD, VerifiedClaimPrincipal},
    },
    identity_state::{self, ClaimState},
};

pub(crate) fn handle(
    client: &HostedClient,
    method: &str,
    principal: &VerifiedClaimPrincipal,
    body: &[u8],
) -> Result<Vec<u8>> {
    let signer = client
        .claim_proof_signer()
        .context("foreground claim signer unavailable")?;
    handle_with_authority(
        signer,
        client.claim_authority_token(),
        method,
        principal,
        body,
    )
}

pub(crate) fn handle_with_authority(
    signer: &crypto::Ed25519Signer,
    authority_token: &[u8],
    method: &str,
    principal: &VerifiedClaimPrincipal,
    body: &[u8],
) -> Result<Vec<u8>> {
    let _guard = identity_state::write_lock()?;
    let mut state = identity_state::load_while_locked()?.context("claim state unavailable")?;
    let now = chrono::Utc::now();
    if state.owner_id.to_string() != principal.subject
        || state.authorization_hash() != principal.authorization_hash
        || !state.consent_unexpired(now.timestamp_millis())
    {
        bail!("claim ceremony unavailable or expired");
    }
    let operation = match method {
        CLAIM_PREPARE_METHOD => PrepareAccountClaimRequest::decode(body)?.client_operation_id,
        CLAIM_SIGN_METHOD => SignAccountClaimRequest::decode(body)?.client_operation_id,
        _ => bail!("unknown native claim method"),
    };
    uuid::Uuid::parse_str(&operation).context("claim operation must be a UUID")?;
    if let Some(response) = state.cached_command(method, &operation, body)? {
        return Ok(response);
    }
    if !state.is_active(now.timestamp_millis()) {
        bail!("claim ceremony already completed");
    }
    let root = super::owner_root::load_recorded_root(&state)?
        .context("claimable owner root unavailable")?;
    let root_body = root.root.as_ref().context("missing owner root body")?;
    if root_body.account_uuid != state.owner_id.as_bytes()
        || repo::seq0_authority_public_key(&root)? != signer.public_key()
        || root_body.claimable_until_unix_seconds <= now.timestamp()
    {
        bail!("claim signer, account or root lifetime changed");
    }
    let response = match method {
        CLAIM_PREPARE_METHOD => {
            let request = PrepareAccountClaimRequest::decode(body)?;
            if request.browser_public_key != principal.browser_public_key
                || request.handle.trim().is_empty()
                || request.handle.len() > 256
                || !state.prepare_browser(&request.handle, &request.browser_public_key)
            {
                bail!("claim preparation conflicts with this browser or handle");
            }
            let token =
                std::str::from_utf8(authority_token).context("invalid local claim credential")?;
            let metadata = headless_token_metadata(token)?;
            let mut expiry = (now.timestamp() + 300)
                .min(state.expires_at_millis / 1000)
                .min(root_body.claimable_until_unix_seconds);
            if let Some(value) = metadata.expires_at {
                expiry = expiry.min(chrono::DateTime::parse_from_rfc3339(&value)?.timestamp());
            }
            if expiry <= now.timestamp() {
                bail!("claim credential expired");
            }
            let expires_at =
                chrono::DateTime::from_timestamp(expiry, 0).context("invalid claim expiry")?;
            let delegated = attenuate_for_agent(
                token,
                AgentAttenuation {
                    agent_id: format!("account-claim-{}", request.client_operation_id),
                    expires_at,
                    allowed_operations: Some(vec![
                        "BeginRegistration".into(),
                        "CompleteRegistration".into(),
                    ]),
                    allowed_resources: None,
                    declared_scopes: Vec::new(),
                },
                signer,
                &request.browser_public_key,
            )?;
            let ceremony_biscuit =
                biscuit_auth::UnverifiedBiscuit::from_base64(delegated.as_bytes())?.to_vec()?;
            PrepareAccountClaimResponse {
                receipt: Some(receipt(&state, &operation, now)?),
                owner_root: Some(root),
                display_name: state.pet_name.clone(),
                ceremony_biscuit,
                expires_at: Some(prost_types::Timestamp {
                    seconds: expiry,
                    nanos: 0,
                }),
                ..Default::default()
            }
            .encode_to_vec()
        }
        CLAIM_SIGN_METHOD => {
            let request = SignAccountClaimRequest::decode(body)?;
            let registration = request
                .registration
                .context("claim registration is required")?;
            if registration.owner.is_some()
                || registration.caller_public_key != principal.browser_public_key
                || !state.accepts_browser(&registration.caller_public_key)
                || registration.client_operation_id.is_empty()
                || registration
                    .challenge
                    .as_ref()
                    .is_none_or(|challenge| challenge.id.is_empty())
                || registration.passkey.as_ref().is_none_or(|passkey| {
                    passkey.credential_id.is_empty()
                        || passkey.client_data_json.is_empty()
                        || passkey.attestation_object.is_empty()
                })
                || registration.device_binding.is_none()
            {
                bail!(
                    "registration must use the prepared browser key and an unclaimed passkey ceremony"
                );
            }
            let proposed = request
                .proposed_transition
                .context("claim transition proposal required")?;
            let valid_from = proposed
                .transition
                .as_ref()
                .context("missing transition body")?
                .valid_from_unix_seconds;
            if valid_from.abs_diff(now.timestamp()) > 60 {
                bail!("claim transition clock is outside the ceremony window");
            }
            // Weft validates the challenge account, handle and passkey proof.
            // Here only the proved browser key and exact root handoff are signed.
            let transition = repo::sign_proposed_account_claim(
                signer,
                &root,
                &proposed,
                &registration.caller_public_key,
            )?;
            if !state.finish_browser_claim(&registration.caller_public_key) {
                bail!("browser preparation changed");
            }
            SignAccountClaimResponse {
                receipt: Some(receipt(&state, &operation, now)?),
                transition: Some(transition),
            }
            .encode_to_vec()
        }
        _ => bail!("unknown native claim method"),
    };
    state.remember_command(method, &operation, body, &response)?;
    identity_state::store_while_locked(&state)?;
    Ok(response)
}
fn receipt(
    state: &ClaimState,
    operation: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<MutationReceipt> {
    let public_key = hex::decode(&state.node_id).context("invalid device endpoint key")?;
    if public_key.len() != 32 {
        bail!("invalid device endpoint key");
    }
    Ok(MutationReceipt {
        client_operation_id: operation.into(),
        endpoint: Some(EndpointRef {
            public_key,
            kind: EndpointKind::Device as i32,
        }),
        outcome: Some(mutation_receipt::Outcome::Applied(Applied::default())),
        observed_at: Some(prost_types::Timestamp {
            seconds: now.timestamp(),
            nanos: now.timestamp_subsec_nanos() as i32,
        }),
    })
}
