//! Claimable deferred-human owner-root mint for agent login / provision / claim.

use anyhow::{Context, Result, bail};
use api::heddle::api::v1alpha1::{
    BootstrapOwnerRootRequest, SignedOwnerKeyTransition, SignedOwnerRoot,
};
use crypto::{Ed25519Signer, Signer as _};
use prost::Message;
use repo::{
    ClaimDeferredHuman, seq0_authority_public_key, sign_claim_deferred_human,
    sign_claimable_deferred_human_root,
};
use wire::ProtocolError;

use super::{
    hosted::{HostedClient, operation_id::ClientOperationId},
    identity_state::{self, ClaimState},
};

pub(crate) fn mint_and_record_claimable_root(
    state: &mut ClaimState,
    signer: &Ed25519Signer,
    now_unix_seconds: i64,
) -> Result<SignedOwnerRoot> {
    if let Some(existing) = load_recorded_root(state)? {
        let seq0 = seq0_authority_public_key(&existing)?;
        if seq0 != signer.public_key() {
            bail!(
                "stored sequence-0 owner root is not this device/proof key; refusing to remint a different authority"
            );
        }
        return Ok(existing);
    }
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).context("minting claimable owner-root nonce")?;
    let signed = sign_claimable_deferred_human_root(
        signer,
        *state.owner_id.as_bytes(),
        nonce,
        now_unix_seconds,
    )?;
    let seq0 = seq0_authority_public_key(&signed)?.to_vec();
    state.record_claimable_owner_root(&seq0, &signed.encode_to_vec());
    Ok(signed)
}

pub(crate) fn persist_claimable_root(state: &ClaimState) -> Result<()> {
    identity_state::store(state)
}

pub(crate) fn load_recorded_root(state: &ClaimState) -> Result<Option<SignedOwnerRoot>> {
    let Some(hex) = state.signed_owner_root_hex.as_deref() else {
        return Ok(None);
    };
    let bytes = hex::decode(hex).context("decode stored claimable owner root")?;
    let signed =
        SignedOwnerRoot::decode(bytes.as_slice()).context("parse stored claimable owner root")?;
    Ok(Some(signed))
}

pub(crate) fn stored_seq0_public_key() -> Result<Option<Vec<u8>>> {
    Ok(identity_state::load()?.and_then(|state| state.seq0_public_key()))
}

pub(crate) async fn upload_claimable_root(
    client: &mut HostedClient,
    signed: SignedOwnerRoot,
) -> Result<()> {
    let operation_id =
        ClientOperationId::fresh("heddle.api.v1alpha1.OwnerAuthorizationService/BootstrapOwnerRoot");
    match client
        .bootstrap_owner_root(BootstrapOwnerRootRequest {
            owner_root: Some(signed),
            approval: None,
            client_operation_id: operation_id.to_wire(),
        })
        .await
    {
        Ok(_) => Ok(()),
        Err(ProtocolError::AlreadyExists(_)) => Ok(()),
        Err(ProtocolError::InvalidState(message)) if already_installed(&message) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Build ClaimDeferredHuman. Do not send this as ClaimAgentOwner / a
/// replacement sequence-0 OwnerRootInstall — those rewrite genesis.
pub(crate) fn build_claim_deferred_human(
    agent: &Ed25519Signer,
    human: &Ed25519Signer,
    signed_root: &SignedOwnerRoot,
    next_recovery_policy: api::heddle::api::v1alpha1::RecoveryPolicy,
    next_guardian_signers: &[Ed25519Signer],
    now_unix_seconds: i64,
) -> Result<SignedOwnerKeyTransition> {
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).context("minting ClaimDeferredHuman nonce")?;
    sign_claim_deferred_human(ClaimDeferredHuman {
        current_authority: agent,
        next_authority: human,
        signed_root,
        next_recovery_policy,
        next_guardian_signers,
        now_unix_seconds,
        nonce,
    })
}

fn already_installed(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("already") && (lowered.contains("owner") || lowered.contains("root"))
}
