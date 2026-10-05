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
/// Typed native and import bundles share verified lineage selection, while
/// retaining their separate validators and disclosure contracts.
pub trait PublicEvidence {
    fn validate(&self) -> Result<(), Reject>;
    fn public_history(&self) -> repo::thread_replication::delegated_import::PublicHistory<'_>;
    fn policies(&self) -> &[wire::SignedSpoolPolicyRecord];
    fn statements(&self) -> &[host::SignedHostedWitnessStatementV1];
    fn binding_selectors(&self) -> Vec<(Vec<u8>, u64)> {
        Vec::new()
    }
    fn imported(&self) -> Option<&wire::ImportPublicProofBundleV1> {
        None
    }
    fn native(&self) -> Option<&wire::NativePublicProofBundleV1> {
        None
    }
    fn delegations(&self) -> &[wire::SignedImportJobDelegationV1] {
        &[]
    }
    fn member_permission(&self) -> Option<&wire::SignedImportMemberPermissionV1> {
        None
    }
    fn native_envelope(
        &self,
        statement: &host::HostedWitnessStatementV1,
    ) -> Option<(&[u8], Vec<Vec<u8>>)>;
}
fn authority_envelope<'a>(
    statement: &host::HostedWitnessStatementV1,
    authorities: &'a [wire::ImportAuthorityWitnessV1],
    landings: &'a [wire::HostedLandingWitnessV1],
) -> Option<(&'a [u8], Vec<Vec<u8>>)> {
    if statement.purpose == 2 {
        let p = authorities.iter().find(|p| {
            api::hybrid_codec::canonical(*p).is_ok_and(|b| b == statement.canonical_payload)
        })?;
        Some((
            &p.authority_envelope,
            p.original
                .as_ref()?
                .signatures
                .iter()
                .map(|s| s.public_key.clone())
                .collect(),
        ))
    } else if statement.purpose == 4 {
        let p = landings.iter().find(|p| {
            api::hybrid_codec::canonical(*p).is_ok_and(|b| b == statement.canonical_payload)
        })?;
        Some((
            &p.authority_envelope,
            vec![p.request.as_ref()?.signature.as_ref()?.public_key.clone()],
        ))
    } else {
        None
    }
}
impl PublicEvidence for wire::ImportPublicProofBundleV1 {
    fn validate(&self) -> Result<(), Reject> {
        api::import_authority::validate_public_bundle(self)
    }
    fn public_history(&self) -> repo::thread_replication::delegated_import::PublicHistory<'_> {
        self.into()
    }
    fn policies(&self) -> &[wire::SignedSpoolPolicyRecord] {
        &self.policies
    }
    fn statements(&self) -> &[host::SignedHostedWitnessStatementV1] {
        &self.statements
    }
    fn imported(&self) -> Option<&wire::ImportPublicProofBundleV1> {
        Some(self)
    }
    fn delegations(&self) -> &[wire::SignedImportJobDelegationV1] {
        &self.delegations
    }
    fn member_permission(&self) -> Option<&wire::SignedImportMemberPermissionV1> {
        self.member_permission.as_ref()
    }
    fn native_envelope(
        &self,
        statement: &host::HostedWitnessStatementV1,
    ) -> Option<(&[u8], Vec<Vec<u8>>)> {
        if statement.purpose != 1 {
            return authority_envelope(
                statement,
                &self.authority_witnesses,
                &self.landing_witnesses,
            );
        }
        let p = self.genesis_witnesses.iter().find(|p| {
            api::hybrid_codec::canonical(*p).is_ok_and(|b| b == statement.canonical_payload)
        })?;
        Some((
            &p.creator_authority_envelope,
            p.original_genesis
                .as_ref()?
                .signatures
                .iter()
                .map(|s| s.public_key.clone())
                .collect(),
        ))
    }
}
impl PublicEvidence for wire::NativePublicProofBundleV1 {
    fn validate(&self) -> Result<(), Reject> {
        api::native_witness::validate_public_bundle(self)
    }
    fn public_history(&self) -> repo::thread_replication::delegated_import::PublicHistory<'_> {
        self.into()
    }
    fn policies(&self) -> &[wire::SignedSpoolPolicyRecord] {
        &self.policies
    }
    fn statements(&self) -> &[host::SignedHostedWitnessStatementV1] {
        &self.statements
    }
    fn native(&self) -> Option<&wire::NativePublicProofBundleV1> {
        Some(self)
    }
    fn binding_selectors(&self) -> Vec<(Vec<u8>, u64)> {
        self.genesis_witnesses
            .iter()
            .filter_map(|p| p.binding.as_ref()?.body.as_ref()?.identity.as_ref())
            .map(|id| (id.owner_state_hash.clone(), id.ownership_transfer_sequence))
            .collect()
    }
    fn native_envelope(
        &self,
        statement: &host::HostedWitnessStatementV1,
    ) -> Option<(&[u8], Vec<Vec<u8>>)> {
        if statement.purpose != 1 {
            return authority_envelope(
                statement,
                &self.authority_witnesses,
                &self.landing_witnesses,
            );
        }
        let p = self.genesis_witnesses.iter().find(|p| {
            api::hybrid_codec::canonical(*p).is_ok_and(|b| b == statement.canonical_payload)
        })?;
        Some((
            &p.creator_authority_envelope,
            p.original_genesis
                .as_ref()?
                .signatures
                .iter()
                .map(|s| s.public_key.clone())
                .collect(),
        ))
    }
}

/// Explicit transport dispatch; an import failure never becomes native history.
#[derive(Clone, Debug, PartialEq)]
pub enum PublicProof {
    Import(Box<wire::ImportPublicProofBundleV1>),
    Native(Box<wire::NativePublicProofBundleV1>),
}
impl From<wire::ImportPublicProofBundleV1> for PublicProof {
    fn from(b: wire::ImportPublicProofBundleV1) -> Self {
        Self::Import(Box::new(b))
    }
}
impl From<wire::NativePublicProofBundleV1> for PublicProof {
    fn from(b: wire::NativePublicProofBundleV1) -> Self {
        Self::Native(Box::new(b))
    }
}
impl PublicProof {
    pub fn witness_set(&self) -> Option<&host::SignedHostedWitnessSetV1> {
        match self {
            Self::Import(b) => b.witness_set.as_ref(),
            Self::Native(b) => b.witness_set.as_ref(),
        }
    }
    pub fn replace_receiver_metadata(&mut self, refreshed: Self) -> Result<(), Reject> {
        match (self, refreshed) {
            (Self::Import(b), Self::Import(r)) => super::history::replace_receiver_metadata(b, *r),
            (Self::Native(b), Self::Native(r)) => {
                super::history::replace_native_receiver_metadata(b, *r)
            }
            _ => Err(Reject::Protocol),
        }
    }
    pub fn encode_to_vec(&self) -> Vec<u8> {
        use prost::Message;
        match self {
            Self::Import(b) => b.encode_to_vec(),
            Self::Native(b) => b.encode_to_vec(),
        }
    }
}
impl PublicEvidence for PublicProof {
    fn validate(&self) -> Result<(), Reject> {
        match self {
            Self::Import(b) => b.validate(),
            Self::Native(b) => b.validate(),
        }
    }
    fn public_history(&self) -> repo::thread_replication::delegated_import::PublicHistory<'_> {
        match self {
            Self::Import(b) => b.as_ref().into(),
            Self::Native(b) => b.as_ref().into(),
        }
    }
    fn policies(&self) -> &[wire::SignedSpoolPolicyRecord] {
        match self {
            Self::Import(b) => &b.policies,
            Self::Native(b) => &b.policies,
        }
    }
    fn statements(&self) -> &[host::SignedHostedWitnessStatementV1] {
        match self {
            Self::Import(b) => &b.statements,
            Self::Native(b) => &b.statements,
        }
    }
    fn binding_selectors(&self) -> Vec<(Vec<u8>, u64)> {
        match self {
            Self::Import(b) => b.binding_selectors(),
            Self::Native(b) => b.binding_selectors(),
        }
    }
    fn imported(&self) -> Option<&wire::ImportPublicProofBundleV1> {
        match self {
            Self::Import(b) => Some(b),
            _ => None,
        }
    }
    fn native(&self) -> Option<&wire::NativePublicProofBundleV1> {
        match self {
            Self::Native(b) => Some(b),
            _ => None,
        }
    }
    fn delegations(&self) -> &[wire::SignedImportJobDelegationV1] {
        match self {
            Self::Import(b) => &b.delegations,
            _ => &[],
        }
    }
    fn member_permission(&self) -> Option<&wire::SignedImportMemberPermissionV1> {
        match self {
            Self::Import(b) => b.member_permission.as_ref(),
            _ => None,
        }
    }
    fn native_envelope(&self, s: &host::HostedWitnessStatementV1) -> Option<(&[u8], Vec<Vec<u8>>)> {
        match self {
            Self::Import(b) => b.native_envelope(s),
            Self::Native(b) => b.native_envelope(s),
        }
    }
}

/// Historical permission is bound to exact authenticated witness order. The
/// caller supplies today's disclosure check, which runs again at commit.
pub struct SelectedAuthority<F, B = wire::ImportPublicProofBundleV1> {
    history: AcceptedHistory,
    bundle: B,
    authorize: F,
}
impl<F> SelectedAuthority<F> {
    pub fn new(
        history: AcceptedHistory,
        bundle: wire::ImportPublicProofBundleV1,
        authorize: F,
    ) -> Self {
        Self {
            history,
            bundle,
            authorize,
        }
    }
}
impl<F> SelectedAuthority<F, wire::NativePublicProofBundleV1> {
    pub fn new_native(
        history: AcceptedHistory,
        bundle: wire::NativePublicProofBundleV1,
        authorize: F,
    ) -> Self {
        Self {
            history,
            bundle,
            authorize,
        }
    }
}
impl<F> SelectedAuthority<F, PublicProof> {
    pub fn from_proof(history: AcceptedHistory, bundle: PublicProof, authorize: F) -> Self {
        Self {
            history,
            bundle,
            authorize,
        }
    }
}
impl<F, B: PublicEvidence> SelectedAuthority<F, B> {
    fn selection<'a>(
        &'a self,
        selected: &'a HistoricalSelection,
    ) -> permission::import_delegation::Selection<'a> {
        permission::import_delegation::Selection {
            owner: &selected.owner,
            keyring: &selected.keyring,
            spool_genesis_digest: self.history.genesis(),
            initial_owner_id: self.history.initial_owner(),
            limits: self.history.limits(),
        }
    }
    fn policy(
        &self,
        statement: &host::HostedWitnessStatementV1,
    ) -> Option<&wire::SignedSpoolPolicy> {
        // The native verifier independently authenticates this exact policy.
        self.bundle.policies().iter().find_map(|signed| {
            let body = signed.body.as_ref()?;
            (body.spool_uuid == statement.spool_uuid
                && body.sequence == statement.policy_sequence
                && body.policy_state_hash == statement.policy_state_hash)
                .then_some(body.policy.as_ref())
                .flatten()
        })
    }
    fn native_envelope(
        &self,
        statement: &host::HostedWitnessStatementV1,
    ) -> Option<(wire::ThreadControlAuthority, Vec<Vec<u8>>)> {
        let (bytes, keys) = self.bundle.native_envelope(statement)?;
        let envelope =
            api::mint_root_association::decode_thread_control_authority_for_verification(bytes)
                .ok()?;
        Some((envelope, keys))
    }

    fn policy_revocations(&self, statement: &host::HostedWitnessStatementV1) -> Option<&[Vec<u8>]> {
        if statement.policy_sequence == 0 && statement.policy_state_hash == [0; 32] {
            // Exactly this authenticated head denotes no policy. A signed
            // record cannot stand in for the implicit genesis policy.
            if self.bundle.policies().iter().any(|record| {
                record.body.as_ref().is_some_and(|body| {
                    body.spool_uuid == statement.spool_uuid
                        && body.sequence == 0
                        && body.policy_state_hash == [0; 32]
                })
            }) {
                return None;
            }
            return Some(&[]);
        }
        self.policy(statement)
            .map(|policy| policy.revoked_key_ids.as_slice())
    }
}
impl<
    B: PublicEvidence,
    F: Fn(
        &B,
        i64,
        &repo::thread_replication::hosted_trust::TrustTransaction<'_>,
    ) -> repo::thread_replication::Result<()>,
> repo::thread_replication::delegated_import::AcceptedAuthority for SelectedAuthority<F, B>
{
    fn authorize_import(
        &self,
        bundle: &wire::ImportPublicProofBundleV1,
        now: i64,
        context: &repo::thread_replication::hosted_trust::TrustTransaction<'_>,
    ) -> repo::thread_replication::Result<()> {
        if self.bundle.imported() != Some(bundle) {
            return Err(Reject::StaleContext.into());
        }
        (self.authorize)(&self.bundle, now, context)
    }
    fn authorize_native(
        &self,
        bundle: &wire::NativePublicProofBundleV1,
        now: i64,
        context: &repo::thread_replication::hosted_trust::TrustTransaction<'_>,
    ) -> repo::thread_replication::Result<()> {
        if self.bundle.native() != Some(bundle) {
            return Err(Reject::StaleContext.into());
        }
        (self.authorize)(&self.bundle, now, context)
    }
    fn for_witness(
        &self,
        statement: &host::HostedWitnessStatementV1,
    ) -> repo::thread_replication::Result<permission::import_delegation::Selection<'_>> {
        let selected = self
            .history
            .for_witness(statement)
            .map_err(authority_error)?;
        Ok(self.selection(selected))
    }
    fn for_native_binding(
        &self,
        binding: &wire::NativeGenesisAuthorityV1,
    ) -> repo::thread_replication::Result<permission::import_delegation::Selection<'_>> {
        let id = binding.identity.as_ref().ok_or(Reject::GenesisBinding)?;
        let selected = self
            .history
            .bindings
            .get(&(
                id.owner_state_hash.clone(),
                id.ownership_transfer_sequence,
                binding.owner_chain_digest.clone(),
            ))
            .ok_or(Reject::Root)?;
        Ok(self.selection(selected))
    }
    fn for_policy(
        &self,
        policy: &wire::SignedPolicyBody,
    ) -> repo::thread_replication::Result<permission::import_delegation::Selection<'_>> {
        let selected = self.history.for_policy(policy).map_err(authority_error)?;
        Ok(self.selection(selected))
    }
    fn import_revoked(
        &self,
        statement: &host::HostedWitnessStatementV1,
        revocation: permission::import_delegation::Revocation<'_>,
    ) -> bool {
        let (Ok(selected), Some(revoked)) = (
            self.history.for_witness(statement),
            self.policy_revocations(statement),
        ) else {
            return true;
        };
        match revocation {
            permission::import_delegation::Revocation::Key(id) => {
                let known = selected
                    .owner
                    .authority_public_keys()
                    .chain(selected.keyring.authority_public_keys().cloned())
                    .chain(
                        self.bundle
                            .delegations()
                            .iter()
                            .filter_map(|d| d.body.as_ref())
                            .flat_map(|d| {
                                [d.delegating_public_key.clone(), d.job_public_key.clone()]
                            }),
                    )
                    .any(|key| api::hybrid_codec::key_id(&key).as_slice() == id);
                !known || revoked.iter().any(|key| key == id)
            }
            permission::import_delegation::Revocation::Cancellation(namespace, id) => {
                // Cancellation status is attested at exact accepted order by
                // the witness; only identifiers actually bound by its signed
                // delegation history can use that historical acceptance.
                namespace != api::import_authority::CANCELLATION_NAMESPACE
                    || !self
                        .bundle
                        .delegations()
                        .iter()
                        .filter_map(|d| d.body.as_ref())
                        .any(|d| d.cancellation_id == id)
                        && !self
                            .bundle
                            .member_permission()
                            .into_iter()
                            .filter_map(|p| p.body.as_ref())
                            .any(|p| p.cancellation_id == id)
            }
        }
    }
    fn native_revoked(
        &self,
        statement: &host::HostedWitnessStatementV1,
        revocation: permission::thread_control_authority::Revocation<'_>,
    ) -> bool {
        let (Ok(_), Some(revoked), Some((envelope, publishers))) = (
            self.history.for_witness(statement),
            self.policy_revocations(statement),
            self.native_envelope(statement),
        ) else {
            return true;
        };
        match revocation {
            permission::thread_control_authority::Revocation::MintRoot(key) => {
                key != envelope.mint_root_public_key
                    || revoked.contains(&api::hybrid_codec::key_id(key).to_vec())
            }
            permission::thread_control_authority::Revocation::Publisher(key) => {
                !publishers.iter().any(|p| p == key)
                    || revoked.contains(&api::hybrid_codec::key_id(key).to_vec())
            }
            permission::thread_control_authority::Revocation::Credential(id) => {
                let keys = biscuit_verifier::parse_ed25519_public_keys_hex(
                    &hex::encode(&envelope.mint_root_public_key),
                    1,
                );
                let inspected = keys.ok().and_then(|keys| {
                    let key = *keys.first()?;
                    let token =
                        biscuit_verifier::signature_v1::verify(&envelope.sealed_biscuit, key)
                            .ok()?;
                    biscuit_verifier::inspect_verified_credential(&token, &key).ok()
                });
                // The authenticated witness attests this exact original's
                // session and signed-block status at first admission. Current
                // delivery credentials never answer historical revocations.
                inspected
                    .is_none_or(|facts| !facts.revocation_identities().any(|known| known == id))
            }
        }
    }
}
fn authority_error(error: impl std::fmt::Display) -> repo::thread_replication::Error {
    repo::thread_replication::Error::Invalid(error.to_string())
}

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
    bindings: BTreeMap<(Vec<u8>, u64, Vec<u8>), HistoricalSelection>,
}

impl AcceptedHistory {
    pub fn from_selected_spool(
        bundle: &wire::ImportPublicProofBundleV1,
        selected: &VerifiedCloneKeyring,
        now_seconds: i64,
        limits: VerificationLimits,
    ) -> Result<Self, Error> {
        Self::from_public(bundle, selected, now_seconds, limits)
    }
    pub fn from_native_spool(
        bundle: &wire::NativePublicProofBundleV1,
        selected: &VerifiedCloneKeyring,
        now_seconds: i64,
        limits: VerificationLimits,
    ) -> Result<Self, Error> {
        Self::from_public(bundle, selected, now_seconds, limits)
    }
    pub fn from_public(
        bundle: &impl PublicEvidence,
        selected: &VerifiedCloneKeyring,
        now_seconds: i64,
        limits: VerificationLimits,
    ) -> Result<Self, Error> {
        bundle.validate()?;
        let public = bundle.public_history();
        let pinned = selected.wire();
        if public.owner_genesis != Some(selected.owner_genesis().signed())
            || !pinned
                .ownership_transfers
                .starts_with(public.ownership_transfers)
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
            bindings: BTreeMap::new(),
        };
        let selectors = bundle
            .statements()
            .iter()
            .map(|signed| {
                let body = signed.body.as_ref().ok_or(Reject::Canonical)?;
                Ok((
                    body.owner_state_hash.clone(),
                    body.ownership_transfer_sequence,
                ))
            })
            .chain(bundle.policies().iter().map(|signed| {
                let body = signed.body.as_ref().ok_or(Reject::Canonical)?;
                Ok((
                    body.owner_state_hash.clone(),
                    body.ownership_transfer_sequence,
                ))
            }))
            .chain(bundle.binding_selectors().into_iter().map(Ok))
            .collect::<Result<std::collections::BTreeSet<_>, Reject>>()?;
        for (hash, sequence) in selectors {
            let count = usize::try_from(sequence).map_err(|_| Reject::Bounds)?;
            let transfers = public
                .ownership_transfers
                .get(..count)
                .ok_or(Reject::Root)?;
            let history = public
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
                public
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
            wire.transfer_owner_histories = public
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
        if let Some(native) = bundle.native() {
            for p in &native.genesis_witnesses {
                let body = p
                    .binding
                    .as_ref()
                    .and_then(|b| b.body.as_ref())
                    .ok_or(Reject::GenesisBinding)?;
                let id = body.identity.as_ref().ok_or(Reject::GenesisBinding)?;
                let witnessed = this
                    .states
                    .get(&(id.owner_state_hash.clone(), id.ownership_transfer_sequence))
                    .ok_or(Reject::Root)?;
                // The binding retains its original keyring endpoint even when
                // accepted authority or other genesis chains have advanced.
                // Every candidate is independently authenticated against the
                // selected immutable root and complete signed transfer prefix.
                let mut resolved = None;
                for history in public
                    .owner_histories
                    .iter()
                    .filter(|h| h.root == witnessed.keyring.wire().owner_root)
                {
                    let mut wire = witnessed.keyring.wire().clone();
                    wire.accepted_transitions = history.accepted_transitions.clone();
                    wire.accepted_state_hash = history.state_hash.clone();
                    let Ok(keyring) =
                        permission::verify_clone_keyring(wire, now_seconds, limits, &[])
                    else {
                        continue;
                    };
                    let selection = permission::import_delegation::Selection {
                        owner: &witnessed.owner,
                        keyring: &keyring,
                        spool_genesis_digest: &this.genesis,
                        initial_owner_id: &this.initial_owner,
                        limits,
                    };
                    if permission::import_delegation::native_lineage(&selection).is_ok_and(
                        |(identity, chain, _)| {
                            body.identity.as_ref() == Some(&identity)
                                && chain == body.owner_chain_digest
                        },
                    ) {
                        if resolved.is_some() {
                            return Err(Reject::Canonical.into());
                        }
                        resolved = Some(HistoricalSelection {
                            owner: witnessed.owner.clone(),
                            keyring,
                        });
                    }
                }
                this.bindings.insert(
                    (
                        id.owner_state_hash.clone(),
                        id.ownership_transfer_sequence,
                        body.owner_chain_digest.clone(),
                    ),
                    resolved.ok_or(Reject::Root)?,
                );
            }
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
#[path = "authority_policy_tests.rs"]
mod policy_tests;

#[cfg(test)]
pub(crate) mod tests {
    use prost::Message;

    use super::*;

    pub(crate) fn bundle() -> wire::ImportPublicProofBundleV1 {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha33.json"))
                .expect("fixed vectors");
        let bytes = hex::decode(
            fixture["wire_vectors"]["complete_export"]["wire_hex"]
                .as_str()
                .expect("wire bytes"),
        )
        .expect("hex");
        wire::ImportPublicProofBundleV1::decode(bytes.as_slice()).expect("public export")
    }
    pub(crate) fn selected(
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

    #[test]
    fn historical_revocation_selectors_are_exact_and_keep_their_namespaces() {
        use permission::{
            import_delegation::Revocation as Import, thread_control_authority::Revocation as Native,
        };
        use repo::thread_replication::delegated_import::AcceptedAuthority as _;
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha33.json"))
                .expect("published vectors");
        let decode = |name: &str, signed: bool| {
            hex::decode(
                fixture[if signed {
                    "signed_vectors"
                } else {
                    "wire_vectors"
                }][name]["wire_hex"]
                    .as_str()
                    .expect("wire"),
            )
            .expect("hex")
        };
        let mut bundle = bundle();
        let native: wire::ImportAuthorityWitnessV1 = wire::ImportAuthorityWitnessV1::decode(
            decode("authority_admission_payload", false).as_slice(),
        )
        .expect("original payload");
        let witness: host::SignedHostedWitnessStatementV1 =
            host::SignedHostedWitnessStatementV1::decode(
                decode("authority_admission", true).as_slice(),
            )
            .expect("native witness");
        bundle.authority_witnesses.push(native.clone());
        bundle.statements.push(witness.clone());
        let limits = VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
        let pinned = selected(&bundle, limits);
        let history = AcceptedHistory::from_selected_spool(&bundle, &pinned, 1350, limits)
            .expect("exact selected history");
        let import_statement = bundle
            .statements
            .iter()
            .find_map(|s| s.body.as_ref().filter(|s| s.purpose == 3))
            .expect("publication witness")
            .clone();
        let delegation = bundle.delegations[0]
            .body
            .as_ref()
            .expect("delegation")
            .clone();
        let authority = SelectedAuthority::new(
            history,
            bundle,
            |_: &wire::ImportPublicProofBundleV1,
             _: i64,
             _: &repo::thread_replication::hosted_trust::TrustTransaction<'_>| Ok(()),
        );
        assert!(!authority.import_revoked(
            &import_statement,
            Import::Key(&api::hybrid_codec::key_id(&delegation.job_public_key))
        ));
        assert!(!authority.import_revoked(
            &import_statement,
            Import::Cancellation(
                api::import_authority::CANCELLATION_NAMESPACE,
                &delegation.cancellation_id
            )
        ));
        assert!(authority.import_revoked(
            &import_statement,
            Import::Cancellation("credential", &delegation.cancellation_id)
        ));
        assert!(
            authority.import_revoked(&import_statement, Import::Key(&delegation.cancellation_id)),
            "cancellation bytes cannot impersonate a key ID"
        );
        assert!(authority.import_revoked(
            &import_statement,
            Import::Cancellation(api::import_authority::CANCELLATION_NAMESPACE, &[0; 32])
        ));
        let statement = witness.body.as_ref().expect("body");
        let envelope = wire::ThreadControlAuthority::decode(native.authority_envelope.as_slice())
            .expect("retained original credential");
        assert!(
            !authority.native_revoked(statement, Native::MintRoot(&envelope.mint_root_public_key))
        );
        assert!(!authority.native_revoked(
            statement,
            Native::Publisher(
                &native.original.as_ref().expect("original").signatures[0].public_key
            )
        ));
        assert!(!authority.native_revoked(statement, Native::Credential("hybrid-native-fixture")));
        assert!(authority.native_revoked(statement, Native::Credential("neighboring-session")));
        assert!(authority.native_revoked(statement, Native::Publisher(&[0; 32])));
        assert!(authority.native_revoked(statement, Native::MintRoot(&[0; 32])));
    }

    struct FixtureAuthority(AcceptedHistory);
    impl FixtureAuthority {
        fn selection<'a>(
            &'a self,
            selected: &'a HistoricalSelection,
        ) -> permission::import_delegation::Selection<'a> {
            permission::import_delegation::Selection {
                owner: &selected.owner,
                keyring: &selected.keyring,
                spool_genesis_digest: self.0.genesis(),
                initial_owner_id: self.0.initial_owner(),
                limits: self.0.limits(),
            }
        }
    }
    impl repo::thread_replication::delegated_import::AcceptedAuthority for FixtureAuthority {
        fn authorize_import(
            &self,
            _: &wire::ImportPublicProofBundleV1,
            now: i64,
            _: &repo::thread_replication::hosted_trust::TrustTransaction<'_>,
        ) -> repo::thread_replication::Result<()> {
            assert_eq!(now, 1_350_000, "use receiver time");
            Ok(())
        }
        fn for_witness(
            &self,
            statement: &host::HostedWitnessStatementV1,
        ) -> repo::thread_replication::Result<permission::import_delegation::Selection<'_>>
        {
            let selected = self
                .0
                .for_witness(statement)
                .map_err(|e| repo::thread_replication::Error::Invalid(e.to_string()))?;
            Ok(self.selection(selected))
        }
        fn for_policy(
            &self,
            policy: &wire::SignedPolicyBody,
        ) -> repo::thread_replication::Result<permission::import_delegation::Selection<'_>>
        {
            let selected = self
                .0
                .for_policy(policy)
                .map_err(|e| repo::thread_replication::Error::Invalid(e.to_string()))?;
            Ok(self.selection(selected))
        }
        fn import_revoked(
            &self,
            _: &host::HostedWitnessStatementV1,
            _: permission::import_delegation::Revocation<'_>,
        ) -> bool {
            // The published fixture has no cancellations or revoked user keys.
            false
        }
        fn native_revoked(
            &self,
            _: &host::HostedWitnessStatementV1,
            _: permission::thread_control_authority::Revocation<'_>,
        ) -> bool {
            // This fixture only authorizes import geneses and job conversions.
            true
        }
    }

    #[test]
    fn staged_context_rechecks_concurrent_durable_revocation_before_install() {
        use repo::thread_replication::{ThreadReplica, hosted_trust::*};

        struct ReceiverClock;
        impl Clock for ReceiverClock {
            fn now_millis(&self) -> repo::thread_replication::Result<i64> {
                Ok(1_350_000)
            }
            fn elapsed_millis(&self) -> repo::thread_replication::Result<u64> {
                Ok(0)
            }
        }
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha33.json"))
                .expect("published fixture");
        fn record<T: Message + Default>(fixture: &serde_json::Value, name: &str) -> T {
            let vector = fixture["wire_vectors"]
                .get(name)
                .or_else(|| fixture["signed_vectors"].get(name))
                .expect("vector");
            T::decode(
                hex::decode(vector["wire_hex"].as_str().expect("wire hex"))
                    .expect("hex")
                    .as_slice(),
            )
            .expect("canonical record")
        }
        let mut prepared = bundle();
        prepared.history_proofs = [
            "genesis_proof",
            "genesis_dev_proof",
            "publication_proof",
            "dev_publication_proof",
        ]
        .map(|name| record(&fixture, name))
        .to_vec();
        let limits = VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
        let pinned = selected(&prepared, limits);
        let authority = FixtureAuthority(
            AcceptedHistory::from_selected_spool(&prepared, &pinned, 1350, limits)
                .expect("independent accepted history"),
        );
        let directory = tempfile::tempdir().expect("receiver");
        let repository = repo::Repository::init_default(directory.path()).expect("repository");
        let root = RootSelection {
            authority: "https://weft.example.test".into(),
            root_id: "descriptor-root-1".into(),
            public_key: hex::decode(
                fixture["keys"]["root"]["public_key_hex"]
                    .as_str()
                    .expect("root"),
            )
            .expect("hex")
            .try_into()
            .expect("root key"),
        };
        select_root(repository.heddle_dir(), &root).expect("independent root");
        select_spool(
            repository.heddle_dir(),
            pinned.owner_genesis().spool_uuid(),
            *authority.0.genesis(),
            *authority.0.initial_owner(),
        )
        .expect("independent Spool");
        let trust = HostedTrust::open(repository.heddle_dir(), &root.authority, ReceiverClock)
            .expect("trust");
        let records: [wire::SignedRecord; 2] = [
            record(&fixture, "converted_main"),
            record(&fixture, "converted_dev"),
        ];
        let install = || {
            ThreadReplica::install_hybrid_import(
                repository.heddle_dir(),
                &trust,
                &prepared.encode_to_vec(),
                &records,
                &authority,
                repository.store(),
                |_| Ok(()),
            )
        };
        let replicas = install().expect("unchanged complete import control");
        let generations = replicas
            .iter()
            .map(|r| r.generation().expect("generation"))
            .collect::<Vec<_>>();
        let second = HostedTrust::open(repository.heddle_dir(), &root.authority, ReceiverClock)
            .expect("independent transfer");
        let revoked = record(&fixture, "revoked_set");
        std::thread::spawn(move || second.mutate(&revoked, |_| Ok(())))
            .join()
            .expect("concurrent writer")
            .expect("persist N+1 revocation");
        assert!(
            matches!(
                install(),
                Err(repo::thread_replication::Error::Hybrid(Reject::HighWater))
            ),
            "staged N must never authorize install after durable N+1 revocation"
        );
        for (replica, generation) in replicas.iter().zip(generations) {
            assert_eq!(replica.generation().expect("unchanged replica"), generation);
        }
        drop(trust);
        let reopened = HostedTrust::open(repository.heddle_dir(), &root.authority, ReceiverClock)
            .expect("restart");
        assert!(matches!(
            ThreadReplica::install_hybrid_import(
                repository.heddle_dir(),
                &reopened,
                &prepared.encode_to_vec(),
                &records,
                &authority,
                repository.store(),
                |_| Ok(()),
            ),
            Err(repo::thread_replication::Error::Hybrid(Reject::HighWater))
        ));
    }
}
