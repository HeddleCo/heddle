use std::collections::BTreeMap;

use crate::owner_authorization::{
    AuthorizationError, AuthorizationKey, RecoverySetup, Result,
    canonical::{OWNER_ROOT_DOMAIN, digest, key_id, nonce, owner_root_body, owner_root_without_id},
    key::verify_signature,
    recovery::validate_wire_policy,
    wire::{
        AuthorizationKeyAlgorithm, AuthorizationVerificationKey, OwnerRoot, RecoveryPolicy,
        SignedOwnerRoot,
    },
};

#[derive(Clone)]
pub(crate) struct HistoricalAuthority {
    pub(crate) key: AuthorizationVerificationKey,
    pub(crate) valid_until: Option<i64>,
}

/// Fully verified owner root plus all accepted offline state.
#[derive(Clone)]
pub struct VerifiedOwnerState {
    pub(crate) signed_root: SignedOwnerRoot,
    pub(crate) owner_id: [u8; 32],
    pub(crate) state_hash: [u8; 32],
    pub(crate) sequence: u64,
    pub(crate) authority_key: AuthorizationVerificationKey,
    pub(crate) recovery_policy: RecoveryPolicy,
    pub(crate) claimable_deferred_human: bool,
    pub(crate) claimable_until_unix_seconds: i64,
    pub(crate) issuers: BTreeMap<[u8; 32], HistoricalAuthority>,
}

impl VerifiedOwnerState {
    /// Stable authorization identity.
    pub fn owner_id(&self) -> [u8; 32] {
        self.owner_id
    }

    /// Hash of the currently accepted state.
    pub fn state_hash(&self) -> [u8; 32] {
        self.state_hash
    }

    /// Current transition sequence, with the root at sequence zero.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Active authority public key.
    pub fn authority_key(&self) -> &AuthorizationVerificationKey {
        &self.authority_key
    }

    /// Active recovery policy.
    pub fn recovery_policy(&self) -> &RecoveryPolicy {
        &self.recovery_policy
    }

    /// Original signed root retained for portable bundles.
    pub fn signed_root(&self) -> &SignedOwnerRoot {
        &self.signed_root
    }

    pub(crate) fn issuer_at(
        &self,
        state_hash: &[u8],
        now_unix_seconds: i64,
    ) -> Result<&AuthorizationVerificationKey> {
        let state_hash: [u8; 32] = state_hash.try_into().map_err(|_| {
            AuthorizationError::Invalid("issuer state hash must be 32 bytes".to_string())
        })?;
        let issuer = self.issuers.get(&state_hash).ok_or_else(|| {
            AuthorizationError::BrokenChain("unknown capability issuer state".to_string())
        })?;
        if issuer
            .valid_until
            .is_some_and(|valid_until| valid_until == 0 || now_unix_seconds > valid_until)
        {
            return Err(AuthorizationError::Expired);
        }
        if self.claimable_deferred_human
            && self.claimable_until_unix_seconds > 0
            && now_unix_seconds > self.claimable_until_unix_seconds
        {
            return Err(AuthorizationError::Expired);
        }
        Ok(&issuer.key)
    }
}

/// Create and self-sign an ordinary human owner root.
pub fn create_human_owner_root(
    account_uuid: [u8; 16],
    authority: &AuthorizationKey,
    recovery: &RecoverySetup,
) -> Result<SignedOwnerRoot> {
    create_owner_root(account_uuid, authority, Some(recovery), false, 0)
}

/// Create a deferred-human origin root with its immutable claim deadline.
pub fn create_deferred_owner_root(
    account_uuid: [u8; 16],
    origin: &AuthorizationKey,
    claimable_until_unix_seconds: i64,
) -> Result<SignedOwnerRoot> {
    if claimable_until_unix_seconds <= 0 {
        return Err(AuthorizationError::Invalid(
            "deferred owner root requires a positive claim deadline".to_string(),
        ));
    }
    create_owner_root(
        account_uuid,
        origin,
        None,
        true,
        claimable_until_unix_seconds,
    )
}

fn create_owner_root(
    account_uuid: [u8; 16],
    authority: &AuthorizationKey,
    recovery: Option<&RecoverySetup>,
    claimable: bool,
    claimable_until: i64,
) -> Result<SignedOwnerRoot> {
    let policy = match recovery {
        Some(setup) => setup.to_wire(authority)?,
        None => RecoveryPolicy {
            threshold: 0,
            guardians: Vec::new(),
        },
    };
    let mut root = OwnerRoot {
        format_version: 1,
        owner_id: Vec::new(),
        account_uuid: account_uuid.to_vec(),
        authority_key: Some(authority.verification_key()),
        recovery_policy: Some(policy),
        claimable_deferred_human: claimable,
        nonce: nonce(),
        claimable_until_unix_seconds: claimable_until,
    };
    root.owner_id = digest(OWNER_ROOT_DOMAIN, &owner_root_without_id(&root)?).to_vec();
    let body = owner_root_body(&root)?;
    let authority_proof = authority.sign(OWNER_ROOT_DOMAIN, &body)?;
    let mut recovery_key_proofs = recovery
        .into_iter()
        .flat_map(RecoverySetup::guardians)
        .map(|guardian| guardian.key().sign(OWNER_ROOT_DOMAIN, &body))
        .collect::<Result<Vec<_>>>()?;
    recovery_key_proofs.sort_by(|left, right| left.signer_key_id.cmp(&right.signer_key_id));
    Ok(SignedOwnerRoot {
        root: Some(root),
        authority_proof: Some(authority_proof),
        recovery_key_proofs,
    })
}

/// Verify all root fields and possession proofs without network access.
pub fn verify_owner_root(signed: &SignedOwnerRoot) -> Result<VerifiedOwnerState> {
    let root = signed
        .root
        .as_ref()
        .ok_or_else(|| AuthorizationError::Invalid("signed owner root has no body".to_string()))?;
    if root.format_version != 1
        || root.owner_id.len() != 32
        || root.account_uuid.len() != 16
        || root.nonce.len() != 32
    {
        return Err(AuthorizationError::Invalid(
            "owner root has invalid v1 field lengths".to_string(),
        ));
    }
    let authority = root.authority_key.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("owner root has no authority key".to_string())
    })?;
    if authority.algorithm != AuthorizationKeyAlgorithm::Ed25519 as i32
        || authority.public_key.len() != 32
    {
        return Err(AuthorizationError::Invalid(
            "owner authority is not 32-byte Ed25519".to_string(),
        ));
    }
    if root.claimable_deferred_human != (root.claimable_until_unix_seconds > 0) {
        return Err(AuthorizationError::Invalid(
            "deferred claim flag and deadline disagree".to_string(),
        ));
    }
    let expected_owner_id = digest(OWNER_ROOT_DOMAIN, &owner_root_without_id(root)?);
    if root.owner_id.as_slice() != expected_owner_id {
        return Err(AuthorizationError::Invalid(
            "owner id does not match the canonical root".to_string(),
        ));
    }

    let policy = root.recovery_policy.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("owner root has no recovery policy".to_string())
    })?;
    validate_wire_policy(policy, &key_id(authority), root.claimable_deferred_human)?;
    let body = owner_root_body(root)?;
    verify_signature(
        authority,
        signed.authority_proof.as_ref().ok_or_else(|| {
            AuthorizationError::Invalid("owner root has no authority proof".to_string())
        })?,
        OWNER_ROOT_DOMAIN,
        &body,
    )?;
    if signed.recovery_key_proofs.len() != policy.guardians.len() {
        return Err(AuthorizationError::Invalid(
            "owner root recovery proof count does not match guardians".to_string(),
        ));
    }
    for (guardian, proof) in policy.guardians.iter().zip(&signed.recovery_key_proofs) {
        verify_signature(
            guardian.key.as_ref().ok_or_else(|| {
                AuthorizationError::Invalid("recovery guardian has no key".to_string())
            })?,
            proof,
            OWNER_ROOT_DOMAIN,
            &body,
        )?;
    }

    let state_hash = digest(OWNER_ROOT_DOMAIN, &body);
    let mut issuers = BTreeMap::new();
    issuers.insert(
        state_hash,
        HistoricalAuthority {
            key: authority.clone(),
            valid_until: None,
        },
    );
    Ok(VerifiedOwnerState {
        signed_root: signed.clone(),
        owner_id: expected_owner_id,
        state_hash,
        sequence: 0,
        authority_key: authority.clone(),
        recovery_policy: policy.clone(),
        claimable_deferred_human: root.claimable_deferred_human,
        claimable_until_unix_seconds: root.claimable_until_unix_seconds,
        issuers,
    })
}
