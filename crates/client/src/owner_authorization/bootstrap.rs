use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

use crate::owner_authorization::{
    AuthorizationError, AuthorizationKey, Result, VerificationLimits,
    canonical::{
        DEFERRED_BOOTSTRAP_DOMAIN, OWNER_ROOT_DOMAIN, deferred_bootstrap_body, digest, key_id,
        owner_root_body,
    },
    key::verify_signature,
    root::verify_owner_root,
    verify_authorization_bundle,
    wire::{
        BootstrapOwnerRootRequest, BootstrapOwnerRootResponse, DeferredOwnerRootApproval,
        OwnerAuthorizationBundle, SignedOwnerRoot, WebAuthnOwnerRootApproval,
        bootstrap_owner_root_request,
    },
};

/// Compute the WebAuthn challenge bound to an exact owner root.
pub fn bootstrap_challenge(
    owner_root: &SignedOwnerRoot,
    server_challenge_nonce: &[u8],
) -> Result<String> {
    if server_challenge_nonce.is_empty() {
        return Err(AuthorizationError::Invalid(
            "bootstrap server challenge nonce is empty".to_string(),
        ));
    }
    let verified = verify_owner_root(owner_root)?;
    let root = verified.signed_root().root.as_ref().expect("verified root");
    let root_body_digest = digest(OWNER_ROOT_DOMAIN, &owner_root_body(root)?);
    let mut hasher = Sha256::new();
    hasher.update(b"heddle-owner-bootstrap-v1");
    hasher.update(root_body_digest);
    hasher.update(server_challenge_nonce);
    Ok(URL_SAFE_NO_PAD.encode(hasher.finalize()))
}

/// Construct a human bootstrap transport after a WebAuthn ceremony.
pub fn create_human_bootstrap(
    owner_root: SignedOwnerRoot,
    approval: WebAuthnOwnerRootApproval,
    client_operation_id: String,
) -> Result<BootstrapOwnerRootRequest> {
    verify_owner_root(&owner_root)?;
    if approval.challenge_id.is_empty()
        || approval.proof.is_none()
        || client_operation_id.is_empty()
    {
        return Err(AuthorizationError::Invalid(
            "human bootstrap approval or operation id is incomplete".to_string(),
        ));
    }
    Ok(BootstrapOwnerRootRequest {
        owner_root: Some(owner_root),
        approval: Some(bootstrap_owner_root_request::Approval::Human(approval)),
        client_operation_id,
    })
}

/// Construct an agent-authorized deferred-human bootstrap transport.
pub fn create_deferred_bootstrap(
    owner_root: SignedOwnerRoot,
    provisioning_authority: OwnerAuthorizationBundle,
    origin_key: &AuthorizationKey,
    client_operation_id: String,
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<BootstrapOwnerRootRequest> {
    let new_state = verify_owner_root(&owner_root)?;
    if !new_state.claimable_deferred_human
        || now_unix_seconds > new_state.claimable_until_unix_seconds
        || key_id(new_state.authority_key()) != origin_key.key_id()
        || client_operation_id.is_empty()
    {
        return Err(AuthorizationError::Invalid(
            "deferred bootstrap root or operation id is invalid".to_string(),
        ));
    }
    let provisioning =
        verify_authorization_bundle(&provisioning_authority, now_unix_seconds, limits)?;
    let signature_body = deferred_bootstrap_body(
        &new_state.state_hash(),
        &provisioning.leaf().capability().capability_id,
        &client_operation_id,
    )?;
    let approval = DeferredOwnerRootApproval {
        provisioning_authority: Some(provisioning_authority),
        origin_key_request_signature: Some(
            origin_key.sign(DEFERRED_BOOTSTRAP_DOMAIN, &signature_body)?,
        ),
    };
    Ok(BootstrapOwnerRootRequest {
        owner_root: Some(owner_root),
        approval: Some(bootstrap_owner_root_request::Approval::DeferredHuman(
            approval,
        )),
        client_operation_id,
    })
}

/// Verify the origin signature and both authorization chains in a deferred request.
pub fn verify_deferred_bootstrap(
    request: &BootstrapOwnerRootRequest,
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<()> {
    let owner_root = request.owner_root.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("bootstrap request has no owner root".to_string())
    })?;
    let new_state = verify_owner_root(owner_root)?;
    if !new_state.claimable_deferred_human
        || now_unix_seconds > new_state.claimable_until_unix_seconds
        || request.client_operation_id.is_empty()
    {
        return Err(AuthorizationError::Invalid(
            "deferred bootstrap root or operation id is invalid".to_string(),
        ));
    }
    let approval = match request.approval.as_ref() {
        Some(bootstrap_owner_root_request::Approval::DeferredHuman(approval)) => approval,
        _ => {
            return Err(AuthorizationError::Invalid(
                "bootstrap request is not deferred-human".to_string(),
            ));
        }
    };
    let bundle = approval.provisioning_authority.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("deferred bootstrap has no provisioning bundle".to_string())
    })?;
    let provisioning = verify_authorization_bundle(bundle, now_unix_seconds, limits)?;
    let signature_body = deferred_bootstrap_body(
        &new_state.state_hash(),
        &provisioning.leaf().capability().capability_id,
        &request.client_operation_id,
    )?;
    verify_signature(
        new_state.authority_key(),
        approval
            .origin_key_request_signature
            .as_ref()
            .ok_or(AuthorizationError::InvalidSignature)?,
        DEFERRED_BOOTSTRAP_DOMAIN,
        &signature_body,
    )
}

/// Check an unsigned acknowledgement against the locally verified root.
pub fn verify_bootstrap_response(
    response: &BootstrapOwnerRootResponse,
    owner_root: &SignedOwnerRoot,
) -> Result<()> {
    let state = verify_owner_root(owner_root)?;
    if response.owner_id.as_slice() != state.owner_id()
        || response.accepted_root_hash.as_slice() != state.state_hash()
    {
        return Err(AuthorizationError::Invalid(
            "bootstrap acknowledgement does not match the submitted root".to_string(),
        ));
    }
    Ok(())
}
