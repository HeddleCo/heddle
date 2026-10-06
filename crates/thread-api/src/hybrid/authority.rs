//! Shared authority selection lives at the durable receiver boundary.
#[cfg(test)]
use api::{
    heddle::api::{common as host, v1alpha2 as wire},
    hybrid_codec::Reject,
};
#[cfg(test)]
use heddleco_capability_verifier::{self as permission, VerificationLimits, VerifiedCloneKeyring};
pub use repo::thread_replication::authority::*;

#[cfg(test)]
fn authority_error(error: impl std::fmt::Display) -> repo::thread_replication::Error {
    repo::thread_replication::Error::Invalid(error.to_string())
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
