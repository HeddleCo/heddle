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
/// Historical permission is bound to exact authenticated witness order. The
/// caller supplies today's disclosure check, which runs again at commit.
pub struct SelectedAuthority<F> {
    history: AcceptedHistory,
    bundle: wire::ImportPublicProofBundleV1,
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
        self.bundle.policies.iter().find_map(|signed| {
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
        let matches =
            |bytes: Result<Vec<u8>, Reject>| bytes.is_ok_and(|b| b == statement.canonical_payload);
        let (bytes, keys) = match statement.purpose {
            1 => {
                let payload = self
                    .bundle
                    .genesis_witnesses
                    .iter()
                    .find(|p| matches(api::hybrid_codec::canonical(*p)))?;
                (
                    &payload.creator_authority_envelope,
                    payload
                        .original_genesis
                        .as_ref()?
                        .signatures
                        .iter()
                        .map(|s| s.public_key.clone())
                        .collect(),
                )
            }
            2 => {
                let payload = self
                    .bundle
                    .authority_witnesses
                    .iter()
                    .find(|p| matches(api::hybrid_codec::canonical(*p)))?;
                (
                    &payload.authority_envelope,
                    payload
                        .original
                        .as_ref()?
                        .signatures
                        .iter()
                        .map(|s| s.public_key.clone())
                        .collect(),
                )
            }
            4 => {
                let payload = self
                    .bundle
                    .landing_witnesses
                    .iter()
                    .find(|p| matches(api::hybrid_codec::canonical(*p)))?;
                (
                    &payload.authority_envelope,
                    vec![
                        payload
                            .request
                            .as_ref()?
                            .signature
                            .as_ref()?
                            .public_key
                            .clone(),
                    ],
                )
            }
            _ => return None,
        };
        let envelope =
            api::mint_root_association::decode_thread_control_authority_for_verification(bytes)
                .ok()?;
        Some((envelope, keys))
    }
}
impl<F: Fn(&wire::ImportPublicProofBundleV1, i64) -> repo::thread_replication::Result<()>>
    repo::thread_replication::delegated_import::AcceptedAuthority for SelectedAuthority<F>
{
    fn authorize_import(
        &self,
        bundle: &wire::ImportPublicProofBundleV1,
        now: i64,
    ) -> repo::thread_replication::Result<()> {
        if bundle != &self.bundle {
            return Err(Reject::StaleContext.into());
        }
        (self.authorize)(bundle, now)
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
        let (Ok(selected), Some(policy)) =
            (self.history.for_witness(statement), self.policy(statement))
        else {
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
                            .delegations
                            .iter()
                            .filter_map(|d| d.body.as_ref())
                            .flat_map(|d| {
                                [d.delegating_public_key.clone(), d.job_public_key.clone()]
                            }),
                    )
                    .any(|key| api::hybrid_codec::key_id(&key).as_slice() == id);
                !known || policy.revoked_key_ids.iter().any(|key| key == id)
            }
            permission::import_delegation::Revocation::Cancellation(namespace, id) => {
                // Cancellation status is attested at exact accepted order by
                // the witness; only identifiers actually bound by its signed
                // delegation history can use that historical acceptance.
                namespace != api::import_authority::CANCELLATION_NAMESPACE
                    || !self
                        .bundle
                        .delegations
                        .iter()
                        .filter_map(|d| d.body.as_ref())
                        .any(|d| d.cancellation_id == id)
                        && !self
                            .bundle
                            .member_permissions
                            .iter()
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
        let (Some(policy), Some((envelope, publishers))) =
            (self.policy(statement), self.native_envelope(statement))
        else {
            return true;
        };
        match revocation {
            permission::thread_control_authority::Revocation::MintRoot(key) => {
                key != envelope.mint_root_public_key
                    || policy
                        .revoked_key_ids
                        .contains(&api::hybrid_codec::key_id(key).to_vec())
            }
            permission::thread_control_authority::Revocation::Publisher(key) => {
                !publishers.iter().any(|p| p == key)
                    || policy
                        .revoked_key_ids
                        .contains(&api::hybrid_codec::key_id(key).to_vec())
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
pub(crate) mod tests {
    use prost::Message;

    use super::*;

    pub(crate) fn bundle() -> wire::ImportPublicProofBundleV1 {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha25.json"))
                .expect("fixed vectors");
        let bytes = hex::decode(
            fixture["wire_vectors"]["complete_renewed_export"]["wire_hex"]
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
            serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha25.json"))
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
            |_: &wire::ImportPublicProofBundleV1, _: i64| Ok(()),
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
            serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha25.json"))
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
            "renewed_publication_proof",
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
