// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    Error, Result,
    canonical::{
        TRANSFER_ACCEPTANCE_DOMAIN, TRANSFER_AUDIT_DOMAIN, TRANSFER_HANDOFF_DOMAIN, digest,
        transfer_acceptance_body, transfer_audit_body, transfer_handoff_body,
    },
    crypto::verify_signature,
    owner::VerifiedOwnerState,
    wire::{ResourceOwnershipTransfer, ResourceTransferAuditRecord},
};

/// Caller-verified owner state used to validate a resource re-anchor.
#[derive(Clone, Copy)]
pub struct TransferOwner<'a> {
    /// Stable account owner UUID.
    pub stable_owner_uuid: &'a [u8; 16],
    /// Current verified cryptographic state for that owner.
    pub state: &'a VerifiedOwnerState,
}

/// A complete, source-signed and destination-accepted ownership transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedResourceTransfer {
    resource_uuid: [u8; 16],
    transfer_sequence: u64,
    source_owner_uuid: [u8; 16],
    destination_owner_uuid: [u8; 16],
}

impl VerifiedResourceTransfer {
    /// Stable resource UUID retained across the transfer.
    #[must_use]
    pub const fn resource_uuid(self) -> [u8; 16] {
        self.resource_uuid
    }

    /// Gap-free transfer sequence.
    #[must_use]
    pub const fn transfer_sequence(self) -> u64 {
        self.transfer_sequence
    }

    /// Prior owner UUID.
    #[must_use]
    pub const fn source_owner_uuid(self) -> [u8; 16] {
        self.source_owner_uuid
    }

    /// New sole owner UUID.
    #[must_use]
    pub const fn destination_owner_uuid(self) -> [u8; 16] {
        self.destination_owner_uuid
    }
}

/// Verify a complete two-party ownership handoff against current owner states.
pub fn verify_resource_transfer(
    transfer: &ResourceOwnershipTransfer,
    expected_resource_uuid: &[u8; 16],
    expected_sequence: u64,
    source: TransferOwner<'_>,
    destination: TransferOwner<'_>,
) -> Result<VerifiedResourceTransfer> {
    let acceptance = transfer
        .acceptance
        .as_ref()
        .ok_or_else(|| Error::BrokenChain("ownership transfer has no acceptance".to_owned()))?;
    let signed = acceptance.signed_handoff.as_ref().ok_or_else(|| {
        Error::BrokenChain("ownership transfer has no signed source handoff".to_owned())
    })?;
    let handoff = signed
        .handoff
        .as_ref()
        .ok_or_else(|| Error::BrokenChain("ownership transfer has no handoff body".to_owned()))?;
    if handoff.format_version != 1
        || handoff.resource_uuid.as_slice() != expected_resource_uuid
        || handoff.transfer_sequence != expected_sequence
        || handoff.source_owner_uuid.as_slice() != source.stable_owner_uuid
        || handoff.destination_owner_uuid.as_slice() != destination.stable_owner_uuid
        || handoff.source_owner_uuid == handoff.destination_owner_uuid
        || handoff.source_owner_key_state_hash.as_slice() != source.state.state_hash()
        || handoff.destination_owner_key_state_hash.as_slice() != destination.state.state_hash()
        || handoff.nonce.len() != 32
    {
        return Err(Error::BrokenChain(
            "ownership transfer resource, sequence, owner, or key state does not match".to_owned(),
        ));
    }
    verify_signature(
        source.state.authority_key(),
        signed
            .source_signature
            .as_ref()
            .ok_or(Error::InvalidSignature)?,
        TRANSFER_HANDOFF_DOMAIN,
        &transfer_handoff_body(handoff)?,
    )?;
    verify_signature(
        destination.state.authority_key(),
        acceptance
            .destination_signature
            .as_ref()
            .ok_or(Error::InvalidSignature)?,
        TRANSFER_ACCEPTANCE_DOMAIN,
        &transfer_acceptance_body(transfer)?,
    )?;
    Ok(VerifiedResourceTransfer {
        resource_uuid: *expected_resource_uuid,
        transfer_sequence: expected_sequence,
        source_owner_uuid: *source.stable_owner_uuid,
        destination_owner_uuid: *destination.stable_owner_uuid,
    })
}

fn owner_for<'a>(owners: &'a [TransferOwner<'a>], uuid: &[u8]) -> Result<TransferOwner<'a>> {
    let matching = owners
        .iter()
        .copied()
        .filter(|owner| owner.stable_owner_uuid.as_slice() == uuid)
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err(Error::BrokenChain(
            "ownership history has missing or duplicate owner state".to_owned(),
        ));
    }
    Ok(matching[0])
}

/// Verify a gap-free append-only ownership audit chain.
///
/// Returns the sole owner UUID after the last committed re-anchor.
pub fn verify_transfer_audit_chain(
    resource_uuid: &[u8; 16],
    initial_owner_uuid: &[u8; 16],
    records: &[ResourceTransferAuditRecord],
    owners: &[TransferOwner<'_>],
) -> Result<[u8; 16]> {
    let mut current_owner = *initial_owner_uuid;
    let mut previous_hash: Option<[u8; 32]> = None;
    for (index, record) in records.iter().enumerate() {
        if record.committed_at_unix_seconds < 0
            || (index == 0 && !record.previous_audit_record_hash.is_empty())
            || (index > 0
                && record.previous_audit_record_hash.as_slice()
                    != previous_hash.expect("previous audit hash"))
            || record.audit_record_hash.len() != 32
        {
            return Err(Error::BrokenChain(
                "ownership audit predecessor or timestamp is invalid".to_owned(),
            ));
        }
        let transfer = record
            .transfer
            .as_ref()
            .ok_or_else(|| Error::BrokenChain("audit record has no transfer".to_owned()))?;
        let handoff = transfer
            .acceptance
            .as_ref()
            .and_then(|value| value.signed_handoff.as_ref())
            .and_then(|value| value.handoff.as_ref())
            .ok_or_else(|| Error::BrokenChain("audit record has incomplete transfer".to_owned()))?;
        if handoff.source_owner_uuid.as_slice() != current_owner {
            return Err(Error::BrokenChain(
                "ownership transfer forks from a non-current owner".to_owned(),
            ));
        }
        let source = owner_for(owners, &handoff.source_owner_uuid)?;
        let destination = owner_for(owners, &handoff.destination_owner_uuid)?;
        let verified = verify_resource_transfer(
            transfer,
            resource_uuid,
            u64::try_from(index)
                .map_err(|_| Error::Invalid("transfer index overflow".to_owned()))?
                .saturating_add(1),
            source,
            destination,
        )?;
        let expected_hash = digest(TRANSFER_AUDIT_DOMAIN, &transfer_audit_body(record)?);
        if record.audit_record_hash.as_slice() != expected_hash {
            return Err(Error::BrokenChain(
                "ownership audit hash does not match canonical record".to_owned(),
            ));
        }
        previous_hash = Some(expected_hash);
        current_owner = verified.destination_owner_uuid();
    }
    Ok(current_owner)
}
