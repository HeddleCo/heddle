// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::{
    Error, Result, VerificationLimits,
    canonical::{
        OWNER_BINDING_DOMAIN, OWNER_ROOT_DOMAIN, OWNER_TRANSITION_DOMAIN, digest, key_id,
        owner_binding_body, owner_root_body, owner_root_without_id, transition_body,
    },
    crypto::{validate_key, verify_digest_signature, verify_signature},
    wire::{
        AuthorizationSignature, AuthorizationVerificationKey, OwnerKeyBinding, OwnerKeyBindingKind,
        OwnerKeyTransitionKind, RecoveryGuardianKind, RecoveryPolicy, SignedOwnerKeyTransition,
        SignedOwnerRoot, SignedSpoolOwnerGenesis,
    },
};

/// Effective veto window when `RecoveryPolicy.window_secs` is absent.
pub const DEFAULT_RECOVERY_WINDOW_SECS: u64 = 604_800;

#[derive(Clone)]
struct HistoricalAuthority {
    key: AuthorizationVerificationKey,
    valid_until: Option<i64>,
}

/// A cryptographically verified owner root plus its linear state history.
#[derive(Clone)]
pub struct VerifiedOwnerState {
    signed_root: SignedOwnerRoot,
    owner_id: [u8; 32],
    state_hash: [u8; 32],
    sequence: u64,
    authority_key: AuthorizationVerificationKey,
    recovery_policy: RecoveryPolicy,
    claimable_deferred_human: bool,
    claimable_until_unix_seconds: i64,
    issuers: BTreeMap<[u8; 32], HistoricalAuthority>,
}

impl VerifiedOwnerState {
    /// Stable cryptographic owner id derived from the signed root.
    #[must_use]
    pub const fn owner_id(&self) -> [u8; 32] {
        self.owner_id
    }

    /// Hash of the currently accepted owner key state.
    #[must_use]
    pub const fn state_hash(&self) -> [u8; 32] {
        self.state_hash
    }

    /// Current transition sequence, with the root at zero.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Active authority public key.
    #[must_use]
    pub const fn authority_key(&self) -> &AuthorizationVerificationKey {
        &self.authority_key
    }

    /// Active recovery policy.
    #[must_use]
    pub const fn recovery_policy(&self) -> &RecoveryPolicy {
        &self.recovery_policy
    }

    /// Original signed root.
    #[must_use]
    pub const fn signed_root(&self) -> &SignedOwnerRoot {
        &self.signed_root
    }

    pub(crate) fn issuer_at(
        &self,
        state_hash: &[u8],
        now_unix_seconds: i64,
    ) -> Result<&AuthorizationVerificationKey> {
        let hash: [u8; 32] = state_hash
            .try_into()
            .map_err(|_| Error::Invalid("issuer state hash must be 32 bytes".to_owned()))?;
        let issuer = self
            .issuers
            .get(&hash)
            .ok_or_else(|| Error::BrokenChain("unknown capability issuer state".to_owned()))?;
        if issuer
            .valid_until
            .is_some_and(|until| until == 0 || now_unix_seconds > until)
        {
            return Err(Error::Expired);
        }
        if self.claimable_deferred_human
            && self.claimable_until_unix_seconds > 0
            && now_unix_seconds > self.claimable_until_unix_seconds
        {
            return Err(Error::Expired);
        }
        Ok(&issuer.key)
    }
}

/// A verified self-asserted account UUID-to-root binding.
#[derive(Clone)]
pub struct VerifiedOwnerBinding {
    stable_owner_uuid: [u8; 16],
    binding: OwnerKeyBinding,
    initial_state: VerifiedOwnerState,
}

impl VerifiedOwnerBinding {
    /// Stable account owner UUID.
    #[must_use]
    pub const fn stable_owner_uuid(&self) -> [u8; 16] {
        self.stable_owner_uuid
    }

    /// Verified initial root state.
    #[must_use]
    pub const fn initial_state(&self) -> &VerifiedOwnerState {
        &self.initial_state
    }

    /// Verified wire binding.
    #[must_use]
    pub const fn binding(&self) -> &OwnerKeyBinding {
        &self.binding
    }
}

/// A self-signed immutable spool-to-initial-owner-key binding suitable for TOFU pinning.
#[derive(Clone)]
pub struct VerifiedSpoolOwnerGenesis {
    signed: SignedSpoolOwnerGenesis,
    spool_uuid: [u8; 16],
    owner_public_key: AuthorizationVerificationKey,
}

impl VerifiedSpoolOwnerGenesis {
    /// Spool UUID authenticated by the genesis owner signature.
    #[must_use]
    pub const fn spool_uuid(&self) -> [u8; 16] {
        self.spool_uuid
    }

    /// Initial owner authority key authenticated for this spool.
    #[must_use]
    pub const fn owner_public_key(&self) -> &AuthorizationVerificationKey {
        &self.owner_public_key
    }

    /// Original verified wire evidence.
    #[must_use]
    pub const fn signed(&self) -> &SignedSpoolOwnerGenesis {
        &self.signed
    }
}

/// Return the semantic veto window committed by a recovery policy.
#[must_use]
pub fn effective_recovery_window(policy: &RecoveryPolicy) -> u64 {
    policy.window_secs.unwrap_or(DEFAULT_RECOVERY_WINDOW_SECS)
}

fn same_recovery_policy(left: &RecoveryPolicy, right: &RecoveryPolicy) -> bool {
    left.threshold == right.threshold
        && left.guardians == right.guardians
        && effective_recovery_window(left) == effective_recovery_window(right)
}

fn validate_recovery_policy(
    policy: &RecoveryPolicy,
    authority_key_id: &[u8; 32],
    allow_empty: bool,
) -> Result<()> {
    if allow_empty && policy.threshold == 0 && policy.guardians.is_empty() {
        return Ok(());
    }
    if policy.threshold == 0 || policy.threshold as usize > policy.guardians.len() {
        return Err(Error::Invalid(
            "recovery threshold is outside the guardian set".to_owned(),
        ));
    }
    let custodial = policy.threshold == 1
        && policy.guardians.len() == 1
        && policy.guardians[0].kind == RecoveryGuardianKind::Weft as i32;
    if policy.threshold < 2 && !custodial {
        return Err(Error::Invalid(
            "recovery threshold below two is not a Weft-only policy".to_owned(),
        ));
    }
    let has_weft = policy
        .guardians
        .iter()
        .any(|guardian| guardian.kind == RecoveryGuardianKind::Weft as i32);
    let has_independent = policy.guardians.iter().any(|guardian| {
        matches!(
            RecoveryGuardianKind::try_from(guardian.kind),
            Ok(RecoveryGuardianKind::Paper | RecoveryGuardianKind::Social)
        )
    });
    if has_weft && !custodial && !has_independent {
        return Err(Error::Invalid(
            "Weft recovery lacks a paper or social co-factor".to_owned(),
        ));
    }

    let mut ids = Vec::with_capacity(policy.guardians.len());
    for guardian in &policy.guardians {
        RecoveryGuardianKind::try_from(guardian.kind)
            .ok()
            .filter(|kind| *kind != RecoveryGuardianKind::Unspecified)
            .ok_or_else(|| Error::Invalid("unknown recovery guardian kind".to_owned()))?;
        let key = guardian
            .key
            .as_ref()
            .ok_or_else(|| Error::Invalid("recovery guardian has no key".to_owned()))?;
        validate_key(key)?;
        let id = key_id(key);
        if &id == authority_key_id {
            return Err(Error::Invalid(
                "authority key cannot also be a recovery guardian".to_owned(),
            ));
        }
        ids.push(id);
    }
    if ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(Error::Invalid(
            "recovery guardians must be unique and sorted by key id".to_owned(),
        ));
    }
    Ok(())
}

/// Verify self-signed spool genesis evidence and return its TOFU-pinnable binding.
pub fn verify_spool_owner_genesis(
    signed: &SignedSpoolOwnerGenesis,
) -> Result<VerifiedSpoolOwnerGenesis> {
    let genesis = signed
        .genesis
        .as_ref()
        .ok_or_else(|| Error::Invalid("signed spool owner genesis has no body".to_owned()))?;
    let spool_uuid: [u8; 16] = genesis
        .spool_uuid
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invalid("genesis spool UUID must be 16 bytes".to_owned()))?;
    let owner_public_key = genesis
        .owner_public_key
        .as_ref()
        .ok_or_else(|| Error::Invalid("spool owner genesis has no owner key".to_owned()))?;
    validate_key(owner_public_key)?;
    let signed_digest: [u8; 32] = Sha256::new()
        .chain_update(&owner_public_key.public_key)
        .chain_update(spool_uuid)
        .finalize()
        .into();
    verify_digest_signature(
        owner_public_key,
        signed
            .owner_signature
            .as_ref()
            .ok_or(Error::InvalidSignature)?,
        &signed_digest,
    )?;
    Ok(VerifiedSpoolOwnerGenesis {
        signed: signed.clone(),
        spool_uuid,
        owner_public_key: owner_public_key.clone(),
    })
}

/// Verify an owner root, its computed ids, and every possession proof.
pub fn verify_owner_root(signed: &SignedOwnerRoot) -> Result<VerifiedOwnerState> {
    let root = signed
        .root
        .as_ref()
        .ok_or_else(|| Error::Invalid("signed owner root has no body".to_owned()))?;
    if root.format_version != 1
        || root.owner_id.len() != 32
        || root.account_uuid.len() != 16
        || root.nonce.len() != 32
    {
        return Err(Error::Invalid(
            "owner root has invalid v1 field lengths".to_owned(),
        ));
    }
    let authority = root
        .authority_key
        .as_ref()
        .ok_or_else(|| Error::Invalid("owner root has no authority key".to_owned()))?;
    validate_key(authority)?;
    if root.claimable_deferred_human != (root.claimable_until_unix_seconds > 0) {
        return Err(Error::Invalid(
            "deferred claim flag and deadline disagree".to_owned(),
        ));
    }
    let expected_owner_id = digest(OWNER_ROOT_DOMAIN, &owner_root_without_id(root)?);
    if root.owner_id.as_slice() != expected_owner_id {
        return Err(Error::Invalid(
            "owner id does not match the canonical root".to_owned(),
        ));
    }
    let policy = root
        .recovery_policy
        .as_ref()
        .ok_or_else(|| Error::Invalid("owner root has no recovery policy".to_owned()))?;
    validate_recovery_policy(policy, &key_id(authority), root.claimable_deferred_human)?;
    let body = owner_root_body(root)?;
    verify_signature(
        authority,
        signed
            .authority_proof
            .as_ref()
            .ok_or(Error::InvalidSignature)?,
        OWNER_ROOT_DOMAIN,
        &body,
    )?;
    if signed.recovery_key_proofs.len() != policy.guardians.len() {
        return Err(Error::Invalid(
            "owner root recovery proof count does not match guardians".to_owned(),
        ));
    }
    for (guardian, proof) in policy.guardians.iter().zip(&signed.recovery_key_proofs) {
        verify_signature(
            guardian
                .key
                .as_ref()
                .ok_or_else(|| Error::Invalid("recovery guardian has no key".to_owned()))?,
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

/// Verify and apply exactly one owner-state transition.
pub fn apply_transition(
    state: &VerifiedOwnerState,
    signed: &SignedOwnerKeyTransition,
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<VerifiedOwnerState> {
    let transition = signed
        .transition
        .as_ref()
        .ok_or_else(|| Error::Invalid("signed transition has no body".to_owned()))?;
    let expected_sequence = state
        .sequence()
        .checked_add(1)
        .ok_or_else(|| Error::BrokenChain("owner transition sequence overflow".to_owned()))?;
    if transition.format_version != 1
        || transition.owner_id.as_slice() != state.owner_id()
        || transition.previous_state_hash.as_slice() != state.state_hash()
        || transition.sequence != expected_sequence
        || transition.nonce.len() != 32
    {
        return Err(Error::BrokenChain(
            "owner id, previous state hash, or sequence does not match".to_owned(),
        ));
    }
    if transition.valid_from_unix_seconds < 0
        || transition.previous_key_valid_until_unix_seconds < 0
        || (transition.previous_key_valid_until_unix_seconds > 0
            && transition.previous_key_valid_until_unix_seconds
                < transition.valid_from_unix_seconds)
        || transition
            .previous_key_valid_until_unix_seconds
            .saturating_sub(transition.valid_from_unix_seconds)
            > limits.max_capability_ttl_seconds()
    {
        return Err(Error::Invalid(
            "transition handover exceeds the capability TTL ceiling".to_owned(),
        ));
    }
    if now_unix_seconds < transition.valid_from_unix_seconds {
        return Err(Error::NotYetValid);
    }
    let kind = OwnerKeyTransitionKind::try_from(transition.kind)
        .ok()
        .filter(|kind| *kind != OwnerKeyTransitionKind::Unspecified)
        .ok_or_else(|| Error::Invalid("unknown transition kind".to_owned()))?;
    let next_authority = transition
        .next_authority_key
        .as_ref()
        .ok_or_else(|| Error::Invalid("transition has no next authority".to_owned()))?;
    validate_key(next_authority)?;
    let next_policy = transition
        .next_recovery_policy
        .as_ref()
        .ok_or_else(|| Error::Invalid("transition has no next recovery policy".to_owned()))?;
    validate_recovery_policy(next_policy, &key_id(next_authority), false)?;
    let body = transition_body(transition)?;
    if signed
        .authorizations
        .iter()
        .any(|proof| proof.signer_key_id.len() != 32)
        || signed
            .authorizations
            .windows(2)
            .any(|pair| pair[0].signer_key_id.as_slice() >= pair[1].signer_key_id.as_slice())
    {
        return Err(Error::InvalidSignature);
    }

    match kind {
        OwnerKeyTransitionKind::Rotate => {
            if !same_recovery_policy(next_policy, state.recovery_policy())
                || signed.authorizations.len() != 1
                || !signed.next_recovery_key_proofs.is_empty()
            {
                return Err(Error::Invalid(
                    "rotation changed recovery policy or has extra proofs".to_owned(),
                ));
            }
            verify_signature(
                state.authority_key(),
                &signed.authorizations[0],
                OWNER_TRANSITION_DOMAIN,
                &body,
            )?;
            verify_signature(
                next_authority,
                signed
                    .next_authority_key_proof
                    .as_ref()
                    .ok_or(Error::InvalidSignature)?,
                OWNER_TRANSITION_DOMAIN,
                &body,
            )?;
        }
        OwnerKeyTransitionKind::Recover => {
            if !same_recovery_policy(next_policy, state.recovery_policy())
                || transition.previous_key_valid_until_unix_seconds != 0
                || !signed.next_recovery_key_proofs.is_empty()
            {
                return Err(Error::Invalid(
                    "recovery changed policy or retained compromised authority".to_owned(),
                ));
            }
            verify_threshold(
                state.recovery_policy(),
                &signed.authorizations.iter().collect::<Vec<_>>(),
                &body,
            )?;
            verify_signature(
                next_authority,
                signed
                    .next_authority_key_proof
                    .as_ref()
                    .ok_or(Error::InvalidSignature)?,
                OWNER_TRANSITION_DOMAIN,
                &body,
            )?;
        }
        OwnerKeyTransitionKind::RecoveryPolicy => {
            if next_authority != state.authority_key() || signed.next_authority_key_proof.is_some()
            {
                return Err(Error::Invalid(
                    "recovery-policy transition changed authority".to_owned(),
                ));
            }
            let guardian_signatures =
                verify_exact_signature(&signed.authorizations, state.authority_key(), &body)?;
            verify_threshold(state.recovery_policy(), &guardian_signatures, &body)?;
            verify_next_guardians(next_policy, &signed.next_recovery_key_proofs, &body)?;
        }
        OwnerKeyTransitionKind::ClaimDeferredHuman => {
            if !state.claimable_deferred_human
                || now_unix_seconds > state.claimable_until_unix_seconds
                || transition.valid_from_unix_seconds > state.claimable_until_unix_seconds
                || effective_recovery_window(next_policy)
                    != effective_recovery_window(state.recovery_policy())
                || signed.authorizations.len() != 1
            {
                return Err(Error::Invalid(
                    "claim transition does not originate from a claimable state".to_owned(),
                ));
            }
            verify_signature(
                state.authority_key(),
                &signed.authorizations[0],
                OWNER_TRANSITION_DOMAIN,
                &body,
            )?;
            verify_signature(
                next_authority,
                signed
                    .next_authority_key_proof
                    .as_ref()
                    .ok_or(Error::InvalidSignature)?,
                OWNER_TRANSITION_DOMAIN,
                &body,
            )?;
            verify_next_guardians(next_policy, &signed.next_recovery_key_proofs, &body)?;
        }
        OwnerKeyTransitionKind::Unspecified => unreachable!("filtered above"),
    }

    let next_hash = digest(OWNER_TRANSITION_DOMAIN, &body);
    let mut next = state.clone();
    next.sequence = transition.sequence;
    next.state_hash = next_hash;
    next.authority_key.clone_from(next_authority);
    next.recovery_policy.clone_from(next_policy);
    if kind == OwnerKeyTransitionKind::ClaimDeferredHuman {
        next.claimable_deferred_human = false;
    }
    next.issuers
        .get_mut(&state.state_hash())
        .expect("verified current issuer")
        .valid_until = Some(transition.previous_key_valid_until_unix_seconds);
    next.issuers.insert(
        next_hash,
        HistoricalAuthority {
            key: next_authority.clone(),
            valid_until: None,
        },
    );
    Ok(next)
}

/// Verify the stateful veto-window timestamp supplied by an accepting service.
///
/// `pending_since_unix_seconds` is not present in the portable protobuf and
/// therefore must come from the caller's trusted pending-operation record.
/// Weft must call this before accepting a RECOVER or RECOVERY_POLICY entry;
/// historical keyring loading cannot reconstruct that state and instead
/// verifies the signed `valid_from_unix_seconds` and rejects use before it.
pub fn verify_transition_timelock(
    state: &VerifiedOwnerState,
    signed: &SignedOwnerKeyTransition,
    pending_since_unix_seconds: i64,
) -> Result<()> {
    let transition = signed
        .transition
        .as_ref()
        .ok_or_else(|| Error::Invalid("signed transition has no body".to_owned()))?;
    let expected_sequence = state
        .sequence()
        .checked_add(1)
        .ok_or_else(|| Error::BrokenChain("owner transition sequence overflow".to_owned()))?;
    if pending_since_unix_seconds < 0
        || transition.owner_id.as_slice() != state.owner_id()
        || transition.previous_state_hash.as_slice() != state.state_hash()
        || transition.sequence != expected_sequence
    {
        return Err(Error::BrokenChain(
            "timelock transition is detached from current owner state".to_owned(),
        ));
    }
    let kind = OwnerKeyTransitionKind::try_from(transition.kind)
        .ok()
        .filter(|kind| *kind != OwnerKeyTransitionKind::Unspecified)
        .ok_or_else(|| Error::Invalid("unknown transition kind".to_owned()))?;
    if matches!(
        kind,
        OwnerKeyTransitionKind::Recover | OwnerKeyTransitionKind::RecoveryPolicy
    ) {
        let window =
            i64::try_from(effective_recovery_window(state.recovery_policy())).map_err(|_| {
                Error::Invalid("recovery window exceeds signed timestamp range".to_owned())
            })?;
        let earliest = pending_since_unix_seconds
            .checked_add(window)
            .ok_or_else(|| Error::Invalid("recovery window timestamp overflows".to_owned()))?;
        if transition.valid_from_unix_seconds < earliest {
            return Err(Error::NotYetValid);
        }
    }
    Ok(())
}

/// Verify the pending-state timelock and then apply one transition atomically.
///
/// Accepting services should prefer this entry point for RECOVER and
/// RECOVERY_POLICY requests. Keyring loaders use [`apply_transition`] because
/// the pending-state start is deliberately not portable history.
pub fn apply_transition_with_timelock(
    state: &VerifiedOwnerState,
    signed: &SignedOwnerKeyTransition,
    now_unix_seconds: i64,
    pending_since_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<VerifiedOwnerState> {
    verify_transition_timelock(state, signed, pending_since_unix_seconds)?;
    apply_transition(state, signed, now_unix_seconds, limits)
}

fn verify_threshold(
    policy: &RecoveryPolicy,
    signatures: &[&AuthorizationSignature],
    body: &[u8],
) -> Result<()> {
    let guardians = policy
        .guardians
        .iter()
        .filter_map(|guardian| guardian.key.as_ref())
        .map(|key| (key_id(key), key))
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    for signature in signatures {
        let id: [u8; 32] = signature
            .signer_key_id
            .as_slice()
            .try_into()
            .map_err(|_| Error::InvalidSignature)?;
        let key = guardians.get(&id).ok_or(Error::InvalidSignature)?;
        if !seen.insert(id) {
            return Err(Error::InvalidSignature);
        }
        verify_signature(key, signature, OWNER_TRANSITION_DOMAIN, body)?;
    }
    if seen.len() < policy.threshold as usize {
        return Err(Error::RecoveryThreshold {
            required: policy.threshold,
            actual: seen.len(),
        });
    }
    Ok(())
}

fn verify_exact_signature<'a>(
    signatures: &'a [AuthorizationSignature],
    key: &AuthorizationVerificationKey,
    body: &[u8],
) -> Result<Vec<&'a AuthorizationSignature>> {
    let expected = key_id(key);
    let matching = signatures
        .iter()
        .filter(|signature| signature.signer_key_id.as_slice() == expected)
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err(Error::InvalidSignature);
    }
    verify_signature(key, matching[0], OWNER_TRANSITION_DOMAIN, body)?;
    Ok(signatures
        .iter()
        .filter(|signature| signature.signer_key_id.as_slice() != expected)
        .collect())
}

fn verify_next_guardians(
    policy: &RecoveryPolicy,
    proofs: &[AuthorizationSignature],
    body: &[u8],
) -> Result<()> {
    if proofs.len() != policy.guardians.len() {
        return Err(Error::Invalid(
            "next recovery proof count does not match policy".to_owned(),
        ));
    }
    for (guardian, proof) in policy.guardians.iter().zip(proofs) {
        verify_signature(
            guardian
                .key
                .as_ref()
                .ok_or_else(|| Error::Invalid("next guardian has no key".to_owned()))?,
            proof,
            OWNER_TRANSITION_DOMAIN,
            body,
        )?;
    }
    Ok(())
}

/// Verify a self-asserted account UUID-to-root binding for an already verified root.
pub fn verify_owner_key_binding(
    binding: &OwnerKeyBinding,
    initial_state: &VerifiedOwnerState,
    expected_stable_owner_uuid: &[u8; 16],
) -> Result<VerifiedOwnerBinding> {
    let root = initial_state
        .signed_root()
        .root
        .as_ref()
        .expect("verified owner root");
    let kind = OwnerKeyBindingKind::try_from(binding.kind)
        .ok()
        .filter(|kind| *kind != OwnerKeyBindingKind::Unspecified)
        .ok_or_else(|| Error::Invalid("unknown owner key binding kind".to_owned()))?;
    let _ = kind;
    let root_key = binding
        .root_public_key
        .as_ref()
        .ok_or_else(|| Error::Invalid("owner key binding has no root key".to_owned()))?;
    if binding.format_version != 1
        || binding.binding_epoch != 1
        || binding.stable_owner_uuid.as_slice() != expected_stable_owner_uuid
        || root.account_uuid.as_slice() != expected_stable_owner_uuid
        || binding.root_state_hash.as_slice() != initial_state.state_hash()
        || root_key != initial_state.authority_key()
        || binding.challenge_nonce.len() != 32
    {
        return Err(Error::Invalid(
            "owner UUID, root key, state hash, or binding epoch does not match".to_owned(),
        ));
    }
    let body = owner_binding_body(binding)?;
    verify_signature(
        root_key,
        binding
            .root_proof_of_possession
            .as_ref()
            .ok_or(Error::InvalidSignature)?,
        OWNER_BINDING_DOMAIN,
        &body,
    )?;
    Ok(VerifiedOwnerBinding {
        stable_owner_uuid: *expected_stable_owner_uuid,
        binding: binding.clone(),
        initial_state: initial_state.clone(),
    })
}
