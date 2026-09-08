//! Local claimable roots and native ownership bootstrap.
use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1::{
    BootstrapOwnershipRequest, PrincipalRef, SignedOwnerRoot, mutation_receipt,
};
use crypto::{Ed25519Signer, Signer as _};
use prost::Message;
use repo::{
    seq0_authority_public_key, sign_agent_claim_binding, sign_claimable_deferred_human_root,
};

use super::{hosted::HostedClient, identity_state::ClaimState};
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

pub(crate) fn load_recorded_root(state: &ClaimState) -> Result<Option<SignedOwnerRoot>> {
    let Some(hex) = state.signed_owner_root_hex.as_deref() else {
        return Ok(None);
    };
    let bytes = hex::decode(hex).context("decode stored claimable owner root")?;
    let signed =
        SignedOwnerRoot::decode(bytes.as_slice()).context("parse stored claimable owner root")?;
    Ok(Some(signed))
}

pub(crate) async fn upload_claimable_root(
    client: &mut HostedClient,
    signer: &Ed25519Signer,
    signed: SignedOwnerRoot,
) -> Result<()> {
    let operation_id = uuid::Uuid::now_v7().to_string();
    let binding = sign_agent_claim_binding(signer, &signed, &operation_id)?;
    let owner_id = uuid::Uuid::from_slice(
        &signed
            .root
            .as_ref()
            .context("missing owner root")?
            .account_uuid,
    )?
    .to_string();
    let native = client.native().await?;
    let result = native
        .api
        .call::<thread_api::rpc::OwnerAuthorizationServiceBootstrapOwnership>(
            &BootstrapOwnershipRequest {
                client_operation_id: operation_id,
                owner: Some(PrincipalRef { id: owner_id }),
                root: Some(signed),
                binding: Some(binding),
            },
        )
        .await?;
    if !matches!(
        result.receipt.and_then(|receipt| receipt.outcome),
        Some(mutation_receipt::Outcome::Applied(_))
    ) {
        bail!("owner bootstrap did not apply");
    }
    Ok(())
}
