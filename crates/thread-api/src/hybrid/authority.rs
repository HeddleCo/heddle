//! Select exact historical owner contexts from independently observed Spool
//! lineage. Public histories supply signatures, never a replacement root.
use std::collections::BTreeMap;

use api::{
    heddle::api::{common as host, v1alpha2 as wire},
    hybrid_codec::Reject,
};
use heddleco_capability_verifier::{
    self as permission, VerificationLimits, VerifiedCloneKeyring, VerifiedOwnerState,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HYBRID accepted authority rejected: {0}")]
    Rejected(#[from] Reject),
    #[error(transparent)]
    Owner(#[from] permission::Error),
}

/// A verified state for one accepted owner/transfer selection. It grants no
/// current device authority and must be used with an authenticated witness.
pub struct HistoricalSelection {
    pub owner: VerifiedOwnerState,
    pub keyring: VerifiedCloneKeyring,
}
pub struct AcceptedHistory {
    genesis: [u8; 32],
    initial_owner: [u8; 32],
    limits: VerificationLimits,
    states: BTreeMap<(Vec<u8>, u64), HistoricalSelection>,
}

impl AcceptedHistory {
    pub fn from_selected_spool(
        bundle: &wire::ImportPublicProofBundleV1,
        selected: &VerifiedCloneKeyring,
        now_seconds: i64,
        limits: VerificationLimits,
    ) -> Result<Self, Error> {
        api::import_authority::validate_public_bundle(bundle)?;
        let pinned = selected.wire();
        if bundle.owner_genesis.as_ref() != Some(selected.owner_genesis().signed())
            || !pinned
                .ownership_transfers
                .starts_with(&bundle.ownership_transfers)
        {
            return Err(Reject::Root.into());
        }
        let genesis = permission::creation::spool_genesis_digest(
            selected
                .owner_genesis()
                .signed()
                .genesis
                .as_ref()
                .ok_or(Reject::Root)?,
        )?;
        let mut this = Self {
            genesis,
            initial_owner: selected.owner_state().owner_id(),
            limits,
            states: BTreeMap::new(),
        };
        let selectors = bundle
            .statements
            .iter()
            .map(|signed| {
                let body = signed.body.as_ref().ok_or(Reject::Canonical)?;
                Ok((
                    body.owner_state_hash.clone(),
                    body.ownership_transfer_sequence,
                ))
            })
            .chain(bundle.policies.iter().map(|signed| {
                let body = signed.body.as_ref().ok_or(Reject::Canonical)?;
                Ok((
                    body.owner_state_hash.clone(),
                    body.ownership_transfer_sequence,
                ))
            }))
            .collect::<Result<std::collections::BTreeSet<_>, Reject>>()?;
        for (hash, sequence) in selectors {
            let count = usize::try_from(sequence).map_err(|_| Reject::Bounds)?;
            let transfers = bundle
                .ownership_transfers
                .get(..count)
                .ok_or(Reject::Root)?;
            let history = bundle
                .owner_histories
                .iter()
                .find(|h| h.state_hash == hash)
                .ok_or(Reject::Root)?;
            let root = history.root.as_ref().ok_or(Reject::Root)?;
            let mut owner = permission::verify_owner_root(root)?;
            for transition in &history.accepted_transitions {
                owner =
                    permission::apply_accepted_transition(&owner, transition, now_seconds, limits)?;
            }
            if owner.state_hash().as_slice() != hash {
                return Err(Reject::Root.into());
            }
            // Only the initial pinned root or the exact destination of a
            // verified prefix handoff may supply this historical owner.
            let expected_root = if let Some(last) = transfers.last() {
                let handoff = last
                    .transfer
                    .as_ref()
                    .and_then(|t| t.acceptance.as_ref())
                    .and_then(|a| a.signed_handoff.as_ref())
                    .and_then(|s| s.handoff.as_ref())
                    .ok_or(Reject::Root)?;
                pinned
                    .transfer_owner_histories
                    .iter()
                    .find(|h| h.state_hash == handoff.destination_owner_key_state_hash)
                    .and_then(|h| h.root.as_ref())
                    .ok_or(Reject::Root)?
            } else {
                pinned.owner_root.as_ref().ok_or(Reject::Root)?
            };
            if root != expected_root {
                return Err(Reject::Root.into());
            }
            let initial_history = if let Some(first) = transfers.first() {
                let handoff = first
                    .transfer
                    .as_ref()
                    .and_then(|t| t.acceptance.as_ref())
                    .and_then(|a| a.signed_handoff.as_ref())
                    .and_then(|s| s.handoff.as_ref())
                    .ok_or(Reject::Root)?;
                bundle
                    .owner_histories
                    .iter()
                    .find(|h| h.state_hash == handoff.source_owner_key_state_hash)
                    .ok_or(Reject::Root)?
            } else {
                history
            };
            if initial_history.root != pinned.owner_root {
                return Err(Reject::Root.into());
            }
            let mut wire = pinned.clone();
            wire.accepted_transitions = initial_history.accepted_transitions.clone();
            wire.accepted_state_hash = initial_history.state_hash.clone();
            wire.ownership_transfers = transfers.to_vec();
            // The transfer verifier resolves exact signed parties itself.
            let mut party_states = std::collections::BTreeSet::new();
            for transfer in transfers {
                let handoff = transfer
                    .transfer
                    .as_ref()
                    .and_then(|t| t.acceptance.as_ref())
                    .and_then(|a| a.signed_handoff.as_ref())
                    .and_then(|s| s.handoff.as_ref())
                    .ok_or(Reject::Root)?;
                party_states.insert(&handoff.source_owner_key_state_hash);
                party_states.insert(&handoff.destination_owner_key_state_hash);
            }
            wire.transfer_owner_histories = bundle
                .owner_histories
                .iter()
                .filter(|h| party_states.contains(&h.state_hash))
                .cloned()
                .collect();
            let keyring = permission::verify_clone_keyring(wire, now_seconds, limits, &[])?;
            keyring.verify_current_owner(&owner, now_seconds, limits)?;
            this.states
                .insert((hash, sequence), HistoricalSelection { owner, keyring });
        }
        Ok(this)
    }
    pub fn for_witness(
        &self,
        statement: &host::HostedWitnessStatementV1,
    ) -> Result<&HistoricalSelection, Error> {
        let selected = self
            .states
            .get(&(
                statement.owner_state_hash.clone(),
                statement.ownership_transfer_sequence,
            ))
            .ok_or(Reject::Root)?;
        if statement.spool_uuid != selected.keyring.owner_genesis().spool_uuid()
            || statement.spool_genesis_digest != self.genesis
            || statement.owner_id != selected.owner.owner_id()
        {
            return Err(Reject::Root.into());
        }
        Ok(selected)
    }
    pub fn for_policy(
        &self,
        policy: &wire::SignedPolicyBody,
    ) -> Result<&HistoricalSelection, Error> {
        let selected = self
            .states
            .get(&(
                policy.owner_state_hash.clone(),
                policy.ownership_transfer_sequence,
            ))
            .ok_or(Reject::Root)?;
        if policy.spool_uuid != selected.keyring.owner_genesis().spool_uuid()
            || policy.owner_id != selected.owner.owner_id()
        {
            return Err(Reject::Root.into());
        }
        Ok(selected)
    }
    pub fn genesis(&self) -> &[u8; 32] {
        &self.genesis
    }
    pub fn initial_owner(&self) -> &[u8; 32] {
        &self.initial_owner
    }
    pub fn limits(&self) -> VerificationLimits {
        self.limits
    }
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;

    fn bundle() -> wire::ImportPublicProofBundleV1 {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha18.json"))
                .expect("fixed vectors");
        let bytes = hex::decode(
            fixture["wire_vectors"]["complete_renewed_export"]["wire_hex"]
                .as_str()
                .expect("wire bytes"),
        )
        .expect("hex");
        wire::ImportPublicProofBundleV1::decode(bytes.as_slice()).expect("public export")
    }
    fn selected(
        bundle: &wire::ImportPublicProofBundleV1,
        limits: VerificationLimits,
    ) -> VerifiedCloneKeyring {
        let history = &bundle.owner_histories[0];
        let root = history.root.as_ref().expect("owner root");
        let wire = wire::CloneAuthorizationKeyring {
            format_version: 1,
            spool_uuid: bundle
                .owner_genesis
                .as_ref()
                .expect("genesis")
                .genesis
                .as_ref()
                .expect("body")
                .spool_uuid
                .clone(),
            canonical_spool_path_segments: vec!["acme".into(), "imports".into()],
            pin: Some(wire::CloneOwnerPin {
                kind: wire::CloneOwnerPinKind::CloneTofu as i32,
                expected_owner_id: root.root.as_ref().expect("root body").owner_id.clone(),
                first_seen_unix_seconds: 1100,
            }),
            owner_root: Some(root.clone()),
            accepted_transitions: history.accepted_transitions.clone(),
            accepted_state_hash: history.state_hash.clone(),
            owner_genesis: bundle.owner_genesis.clone(),
            ownership_transfers: bundle.ownership_transfers.clone(),
            transfer_owner_histories: vec![],
        };
        permission::verify_clone_keyring(wire, 1200, limits, &[])
            .expect("independent Spool observation")
    }
    #[test]
    fn carried_history_cannot_replace_the_independently_selected_spool() {
        let original = bundle();
        let limits = VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
        let selected = selected(&original, limits);
        let accepted = AcceptedHistory::from_selected_spool(&original, &selected, 1200, limits)
            .expect("exact original history");
        for signed in &original.statements {
            accepted
                .for_witness(signed.body.as_ref().expect("body"))
                .expect("accepted owner selection");
        }
        let mut substituted = original.clone();
        substituted
            .owner_genesis
            .as_mut()
            .expect("genesis")
            .genesis
            .as_mut()
            .expect("body")
            .spool_uuid[0] ^= 1;
        assert!(matches!(
            AcceptedHistory::from_selected_spool(&substituted, &selected, 1200, limits),
            Err(Error::Rejected(Reject::Root))
        ));
        let mut foreign = original.statements[0]
            .body
            .as_ref()
            .expect("witness")
            .clone();
        foreign.spool_uuid[0] ^= 1;
        assert!(accepted.for_witness(&foreign).is_err());
        AcceptedHistory::from_selected_spool(&original, &selected, 1200, limits)
            .expect("original evidence remains usable");
    }
}
