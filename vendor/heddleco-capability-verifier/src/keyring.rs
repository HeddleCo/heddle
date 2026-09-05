// SPDX-License-Identifier: MIT OR Apache-2.0

use prost::Message;

use crate::{
    Error, Result, VerificationLimits,
    capability::validate_path_segments,
    owner::{
        VerifiedOwnerState, VerifiedSpoolOwnerGenesis, apply_transition, verify_owner_root,
        verify_spool_owner_genesis,
    },
    transfer::{TransferOwner, verify_transfer_audit_chain},
    wire::{CloneAuthorizationKeyring, CloneOwnerPinKind},
};

/// A clone keyring after genesis, root, transitions, and ownership history verify.
pub struct VerifiedCloneKeyring {
    wire: CloneAuthorizationKeyring,
    owner_state: VerifiedOwnerState,
    owner_genesis: VerifiedSpoolOwnerGenesis,
    current_owner_uuid: [u8; 16],
}

impl VerifiedCloneKeyring {
    /// Original verified wire object.
    #[must_use]
    pub const fn wire(&self) -> &CloneAuthorizationKeyring {
        &self.wire
    }

    /// Verified state reached by the genesis owner's transition chain.
    #[must_use]
    pub const fn owner_state(&self) -> &VerifiedOwnerState {
        &self.owner_state
    }

    /// Verified immutable spool genesis binding.
    #[must_use]
    pub const fn owner_genesis(&self) -> &VerifiedSpoolOwnerGenesis {
        &self.owner_genesis
    }

    /// Sole owner UUID after every verified ownership re-anchor.
    #[must_use]
    pub const fn current_owner_uuid(&self) -> [u8; 16] {
        self.current_owner_uuid
    }
}

fn verify_pin(keyring: &CloneAuthorizationKeyring, state: &VerifiedOwnerState) -> Result<()> {
    let pin = keyring
        .pin
        .as_ref()
        .ok_or_else(|| Error::Invalid("clone keyring has no owner pin".to_owned()))?;
    let kind = CloneOwnerPinKind::try_from(pin.kind)
        .ok()
        .filter(|kind| *kind != CloneOwnerPinKind::Unspecified)
        .ok_or_else(|| Error::Invalid("unknown clone pin kind".to_owned()))?;
    if !matches!(
        kind,
        CloneOwnerPinKind::LocalCreation
            | CloneOwnerPinKind::InvitationFingerprint
            | CloneOwnerPinKind::CloneTofu
    ) || pin.expected_owner_id.as_slice() != state.owner_id()
        || pin.first_seen_unix_seconds < 0
    {
        return Err(Error::Invalid(
            "clone owner pin does not match the signed root".to_owned(),
        ));
    }
    Ok(())
}

/// Verify a typed clone keyring from its self-signed spool genesis forward.
pub fn verify_clone_keyring(
    keyring: CloneAuthorizationKeyring,
    now_unix_seconds: i64,
    limits: VerificationLimits,
    transfer_owners: &[TransferOwner<'_>],
) -> Result<VerifiedCloneKeyring> {
    if keyring.encoded_len() > limits.max_bundle_bytes() {
        return Err(Error::TooLarge {
            limit: limits.max_bundle_bytes(),
        });
    }
    if keyring.accepted_transitions.len() > VerificationLimits::MAX_TRANSITIONS
        || keyring.ownership_transfers.len() > VerificationLimits::MAX_TRANSITIONS
    {
        return Err(Error::TooLarge {
            limit: VerificationLimits::MAX_TRANSITIONS,
        });
    }
    if keyring.format_version != 1
        || keyring.spool_uuid.len() != 16
        || keyring.accepted_state_hash.len() != 32
    {
        return Err(Error::Invalid(
            "clone keyring has invalid v1 fixed-width fields".to_owned(),
        ));
    }
    validate_path_segments(&keyring.canonical_spool_path_segments)?;
    let spool_uuid: [u8; 16] = keyring
        .spool_uuid
        .as_slice()
        .try_into()
        .expect("checked spool UUID");
    let owner_genesis = verify_spool_owner_genesis(
        keyring
            .owner_genesis
            .as_ref()
            .ok_or_else(|| Error::Invalid("clone keyring has no owner genesis".to_owned()))?,
    )?;
    if owner_genesis.spool_uuid() != spool_uuid {
        return Err(Error::BrokenChain(
            "clone keyring genesis names another spool".to_owned(),
        ));
    }

    let mut state = verify_owner_root(
        keyring
            .owner_root
            .as_ref()
            .ok_or_else(|| Error::Invalid("clone keyring has no owner root".to_owned()))?,
    )?;
    if state.authority_key() != owner_genesis.owner_public_key() {
        return Err(Error::BrokenChain(
            "owner root is not rooted at the spool genesis key".to_owned(),
        ));
    }
    verify_pin(&keyring, &state)?;
    for transition in &keyring.accepted_transitions {
        state = apply_transition(&state, transition, now_unix_seconds, limits)?;
    }
    if keyring.accepted_state_hash.as_slice() != state.state_hash() {
        return Err(Error::BrokenChain(
            "accepted state hash does not match transition history".to_owned(),
        ));
    }

    let initial_owner_uuid: [u8; 16] = state
        .signed_root()
        .root
        .as_ref()
        .expect("verified owner root")
        .account_uuid
        .as_slice()
        .try_into()
        .expect("verified account UUID");
    if let Some(owner) = transfer_owners
        .iter()
        .find(|owner| owner.stable_owner_uuid == &initial_owner_uuid)
    {
        if owner.state.state_hash() != state.state_hash() {
            return Err(Error::BrokenChain(
                "transfer directory disagrees with the keyring owner state".to_owned(),
            ));
        }
    }
    let current_owner_uuid = verify_transfer_audit_chain(
        &spool_uuid,
        &initial_owner_uuid,
        &keyring.ownership_transfers,
        transfer_owners,
    )?;
    Ok(VerifiedCloneKeyring {
        wire: keyring,
        owner_state: state,
        owner_genesis,
        current_owner_uuid,
    })
}

/// Decode canonical keyring protobuf and verify its complete self-rooted history.
pub fn verify_clone_keyring_bytes(
    bytes: &[u8],
    now_unix_seconds: i64,
    limits: VerificationLimits,
    transfer_owners: &[TransferOwner<'_>],
) -> Result<VerifiedCloneKeyring> {
    if bytes.len() > limits.max_bundle_bytes() {
        return Err(Error::TooLarge {
            limit: limits.max_bundle_bytes(),
        });
    }
    let keyring = CloneAuthorizationKeyring::decode(bytes)?;
    if keyring.encode_to_vec() != bytes {
        return Err(Error::NonCanonicalProtobuf);
    }
    verify_clone_keyring(keyring, now_unix_seconds, limits, transfer_owners)
}
