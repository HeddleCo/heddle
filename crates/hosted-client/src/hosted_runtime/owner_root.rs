//! Claimable deferred-human owner-root mint for agent login / provision / claim.

use anyhow::{Context, Result, bail};
use api::heddle::api::v1alpha1::{
    AccessTokenResponse, AuthorizationSignature, AuthorizationVerificationKey,
    BootstrapOwnerRootRequest, BootstrapOwnerRootResponse, OwnerKeyBinding, RecoveryPolicy,
    RegisterPublicKeyRequest, SignedOwnerKeyTransition, SignedOwnerRoot,
};
use crypto::{Ed25519Signer, Signer as _};
use prost::Message;
use repo::{
    ClaimDeferredHuman, seq0_authority_public_key, sign_agent_claim_binding,
    sign_claim_deferred_human, sign_claimable_deferred_human_root,
};

use super::{
    hosted::{HostedClient, HostedError, operation_id::ClientOperationId},
    identity_state::{self, ClaimState},
};

const BOOTSTRAP_OWNER_ROOT: &str =
    "/heddle.api.v1alpha1.OwnerAuthorizationService/BootstrapOwnerRoot";
const REGISTER_PUBLIC_KEY: &str = "/heddle.api.v1alpha1.IdentityService/RegisterPublicKey";

/// Additive weft#1863 field on the raw BootstrapOwnerRoot request.
#[derive(Clone, PartialEq, Message)]
pub struct BootstrapOwnerRootExtension {
    #[prost(message, optional, tag = "5")]
    pub owner_key_binding: Option<OwnerKeyBinding>,
}

/// Additive weft#1863 field on the raw RegisterPublicKey request.
#[derive(Clone, PartialEq, Message)]
pub struct RegisterPublicKeyClaimExtension {
    #[prost(message, optional, tag = "16")]
    pub claim_deferred_human: Option<SignedOwnerKeyTransition>,
}

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

pub(crate) fn encode_bootstrap_owner_root(
    request: &BootstrapOwnerRootRequest,
    binding: &OwnerKeyBinding,
) -> Vec<u8> {
    let mut encoded = request.encode_to_vec();
    encoded.extend_from_slice(
        &BootstrapOwnerRootExtension {
            owner_key_binding: Some(binding.clone()),
        }
        .encode_to_vec(),
    );
    encoded
}

pub(crate) fn encode_register_public_key_claim(
    request: &RegisterPublicKeyRequest,
    transition: &SignedOwnerKeyTransition,
) -> Result<Vec<u8>> {
    if request.owner_root.is_some()
        || request.owner_root_proof_of_possession.is_some()
        || request.owner_key_binding.is_some()
    {
        bail!(
            "RegisterPublicKey claim must not send owner_root, owner_root_proof_of_possession, or owner_key_binding; those replace sequence-0"
        );
    }
    let kind = transition
        .transition
        .as_ref()
        .context("ClaimDeferredHuman has no body")?
        .kind();
    if kind != api::heddle::api::v1alpha1::OwnerKeyTransitionKind::ClaimDeferredHuman {
        bail!("RegisterPublicKey claim extension must be ClaimDeferredHuman, got {kind:?}");
    }
    let mut encoded = request.encode_to_vec();
    encoded.extend_from_slice(
        &RegisterPublicKeyClaimExtension {
            claim_deferred_human: Some(transition.clone()),
        }
        .encode_to_vec(),
    );
    Ok(encoded)
}

pub(crate) async fn upload_claimable_root(
    client: &mut HostedClient,
    signer: &Ed25519Signer,
    signed: SignedOwnerRoot,
) -> Result<()> {
    let operation_id = ClientOperationId::fresh(BOOTSTRAP_OWNER_ROOT);
    let binding = sign_agent_claim_binding(signer, &signed, operation_id.as_str())
        .context("signing AgentClaim owner-key binding for BootstrapOwnerRoot")?;
    let request = BootstrapOwnerRootRequest {
        owner_root: Some(signed),
        approval: None,
        client_operation_id: operation_id.to_wire(),
    };
    let encoded = encode_bootstrap_owner_root(&request, &binding);
    match client
        .call_unary_encoded::<BootstrapOwnerRootResponse>(BOOTSTRAP_OWNER_ROOT, &encoded)
        .await
    {
        Ok(_) => Ok(()),
        Err(HostedError::Call {
            code: api::heddle::api::v1alpha1::CallFailureCode::AlreadyExists,
            ..
        }) => Ok(()),
        Err(HostedError::Call { message, .. }) if already_installed(&message) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Build ClaimDeferredHuman. Do not send this as ClaimAgentOwner / a
/// replacement sequence-0 OwnerRootInstall — those rewrite genesis.
#[derive(Clone, Debug)]
pub(crate) struct BrowserClaimDeferredHuman {
    pub(crate) next_authority_key: AuthorizationVerificationKey,
    pub(crate) next_authority_key_proof: AuthorizationSignature,
    pub(crate) next_recovery_policy: RecoveryPolicy,
    pub(crate) next_recovery_key_proofs: Vec<AuthorizationSignature>,
    pub(crate) valid_from_unix_seconds: i64,
    pub(crate) nonce: [u8; 32],
}

pub(crate) fn build_claim_deferred_human(
    agent: &Ed25519Signer,
    signed_root: &SignedOwnerRoot,
    browser: BrowserClaimDeferredHuman,
) -> Result<SignedOwnerKeyTransition> {
    sign_claim_deferred_human(ClaimDeferredHuman {
        current_authority: agent,
        signed_root,
        next_authority_key: browser.next_authority_key,
        next_authority_key_proof: browser.next_authority_key_proof,
        next_recovery_policy: browser.next_recovery_policy,
        next_recovery_key_proofs: browser.next_recovery_key_proofs,
        valid_from_unix_seconds: browser.valid_from_unix_seconds,
        nonce: browser.nonce,
    })
}

pub(crate) fn prepare_register_public_key_claim(
    state: &mut ClaimState,
    request: RegisterPublicKeyRequest,
    transition: SignedOwnerKeyTransition,
) -> Result<()> {
    let encoded = encode_register_public_key_claim(&request, &transition)?;
    state.record_pending_register_public_key(&encoded);
    Ok(())
}

pub(crate) async fn send_pending_register_public_key_claim(
    client: &mut HostedClient,
) -> Result<Option<AccessTokenResponse>> {
    let encoded = {
        let _guard = identity_state::write_lock()?;
        let Some(mut state) = identity_state::load_while_locked()? else {
            return Ok(None);
        };
        let Some(encoded) = state.take_pending_register_public_key()? else {
            return Ok(None);
        };
        identity_state::store_while_locked(&state)?;
        encoded
    };
    Ok(Some(
        client
            .call_unary_encoded(REGISTER_PUBLIC_KEY, &encoded)
            .await?,
    ))
}

fn already_installed(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("already") && (lowered.contains("owner") || lowered.contains("root"))
}
