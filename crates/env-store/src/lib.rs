// SPDX-License-Identifier: Apache-2.0
//! Local confidential-env store store (ADR 0051 / heddle#999).
//!
//! Typed roots, immutable versions, recipient wrapping, and signed lifecycle
//! records live in the store. The [`PolicyBroker`] authorizes scoped,
//! time-boxed decrypt requests and returns **values, never key material**.
//! `heddle env run` is the only CLI path that consumes those values — it
//! injects them into a child process. Same-UID callers without OS isolation
//! are cooperative, not an adversarial boundary.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod broker;
mod codec;
mod error;
mod ids;
#[cfg(unix)]
mod ipc;
mod store;
mod types;

pub use broker::{
    DecryptGrant, DecryptPurpose, DecryptRequest, PolicyBroker, ProviderHandle, RunSecrets,
};
pub use error::{BrokerDenialReason, Result, EnvStoreError};
pub use ids::{
    AuditRecordId, CiphertextId, LifecycleRecordId, RecipientId, EnvProfileId,
    EnvProfileVersionId,
};
#[cfg(unix)]
pub use ipc::{
    BrokerRequest, BrokerResponse, BrokerSlotValue, bind_broker_socket, broker_socket_path,
    handle_connection, request_run_unwrap, serve_once,
};
pub use store::{EnvStore, SlotWrite, confidential_runtime_source_history_laws};
pub use types::{
    AuditEventKind, AuditRecord, FacetKindWire, LifecycleRecord, LifecycleStatus, ProfileMetadata,
    ProviderCapability, RESERVED_MATERIALIZATION_PATHS, ENV_STORE_SCHEMA_VERSION,
    RecipientDescriptor, EnvProfileRef, EnvProfileVersion, SignatureBlock, SlotMetadata,
    SlotRecord, WrappedDekRecord, is_reserved_materialization_path,
    reserved_materialization_on_disk,
};

#[cfg(test)]
mod tests {
    use crypto::{Ed25519Signer, SoftwareRecipientSecret};
    use heddle_object_model::object::{Attribution, FacetKind, Principal, StateId};
    use tempfile::TempDir;

    use super::*;

    fn signer() -> Ed25519Signer {
        Ed25519Signer::generate().expect("signer")
    }

    fn attribution() -> Attribution {
        Attribution::human(Principal::new("Ada", "ada@example.com"))
    }

    fn open_store() -> (TempDir, EnvStore) {
        let temp = TempDir::new().expect("tempdir");
        let store = EnvStore::open(temp.path()).expect("open store");
        (temp, store)
    }

    fn seeded_broker(
        name: &str,
        slots: Vec<SlotWrite>,
    ) -> (
        TempDir,
        PolicyBroker,
        Ed25519Signer,
        EnvProfileId,
        SoftwareRecipientSecret,
    ) {
        let (temp, store) = open_store();
        let signer = signer();
        let (recipient, secret) = store
            .create_software_recipient(&signer, 1)
            .expect("recipient");
        let profile = store
            .create_profile(name, slots, recipient.recipient_id, attribution(), &signer)
            .expect("create");
        let mut broker = PolicyBroker::new(store, attribution());
        broker.hold_software_secret(recipient.recipient_id, secret.clone());
        (temp, broker, signer, profile.profile_id, secret)
    }

    fn run_request(profile: &str, slots: &[&str], expires_at_ms: i64) -> DecryptRequest {
        DecryptRequest {
            profile: profile.to_string(),
            slots: slots.iter().map(|slot| (*slot).to_string()).collect(),
            expires_at_ms,
            purpose: DecryptPurpose::Run,
            caller: "test".to_string(),
        }
    }

    /// A valid near-future expiry within the broker's TTL ceiling. The broker
    /// reads its own clock, so a request must expire relative to real time.
    fn future() -> i64 {
        crate::store::now_ms().expect("now") + 60_000
    }

    #[test]
    fn ciphertext_in_store_is_not_plaintext() {
        let (_temp, store) = open_store();
        let signer = signer();
        let (recipient, secret) = store
            .create_software_recipient(&signer, 1)
            .expect("recipient");
        let plaintext = b"postgres://user:hunter2@localhost/app";
        let profile = store
            .create_profile(
                "production",
                vec![SlotWrite {
                    name: "DATABASE_URL".to_string(),
                    value: plaintext.to_vec(),
                }],
                recipient.recipient_id,
                attribution(),
                &signer,
            )
            .expect("create");

        let mut saw_ciphertext_file = false;
        for entry in walkdir(store.root()) {
            let bytes = std::fs::read(&entry).expect("read");
            assert!(
                !bytes.windows(plaintext.len()).any(|w| w == plaintext),
                "plaintext leaked into {}",
                entry.display()
            );
            if entry.starts_with(store.root().join("ciphertext")) {
                saw_ciphertext_file = true;
                assert!(!bytes.is_empty());
            }
        }
        assert!(saw_ciphertext_file, "expected dedicated ciphertext files");

        let listed = store.list_slots(profile.profile_id).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "DATABASE_URL");
        let decrypted = store
            .decrypt_slot(
                profile.profile_id,
                "DATABASE_URL",
                recipient.recipient_id,
                &secret,
            )
            .expect("decrypt");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn wrong_recipient_cannot_unwrap() {
        let (_temp, store) = open_store();
        let signer = signer();
        let (recipient, _secret) = store
            .create_software_recipient(&signer, 1)
            .expect("recipient");
        let profile = store
            .create_profile(
                "staging",
                vec![SlotWrite {
                    name: "API_TOKEN".to_string(),
                    value: b"tok_live_abc".to_vec(),
                }],
                recipient.recipient_id,
                attribution(),
                &signer,
            )
            .expect("create");
        let stranger = SoftwareRecipientSecret::generate().expect("stranger");
        store
            .decrypt_slot(
                profile.profile_id,
                "API_TOKEN",
                recipient.recipient_id,
                &stranger,
            )
            .expect_err("wrong recipient must fail");
    }

    #[test]
    fn supersession_advances_signed_lifecycle() {
        let (_temp, store) = open_store();
        let signer = signer();
        let (recipient, secret) = store
            .create_software_recipient(&signer, 1)
            .expect("recipient");
        let profile = store
            .create_profile(
                "local",
                vec![SlotWrite {
                    name: "TOKEN".to_string(),
                    value: b"v1".to_vec(),
                }],
                recipient.recipient_id,
                attribution(),
                &signer,
            )
            .expect("create");
        let first_head = profile.head;
        let updated = store
            .update_slots(
                profile.profile_id,
                vec![SlotWrite {
                    name: "TOKEN".to_string(),
                    value: b"v2".to_vec(),
                }],
                attribution(),
                &signer,
            )
            .expect("update");
        assert_ne!(updated.head, first_head);
        let old = store.load_state(first_head).expect("old state");
        let new = store.load_state(updated.head).expect("new state");
        assert_eq!(old.lifecycle, LifecycleStatus::Superseded);
        assert_eq!(new.lifecycle, LifecycleStatus::Active);
        assert_eq!(new.parent, Some(first_head));
        assert_eq!(new.version, 2);

        let records = store.list_lifecycle(profile.profile_id).expect("lifecycle");
        let statuses: Vec<_> = records.iter().map(|r| (r.from, r.to)).collect();
        assert!(
            statuses.contains(&(None, LifecycleStatus::Staged)),
            "{statuses:?}"
        );
        assert!(
            statuses.contains(&(Some(LifecycleStatus::Staged), LifecycleStatus::Active)),
            "{statuses:?}"
        );
        assert!(
            statuses.contains(&(Some(LifecycleStatus::Active), LifecycleStatus::Superseded)),
            "{statuses:?}"
        );
        for record in &records {
            crypto::verify_payload_signature(
                &crate::codec::lifecycle_signing_payload(record).expect("payload"),
                &record.signature.algorithm,
                &record.signature.public_key,
                &record.signature.signature,
            )
            .expect("lifecycle signature");
        }

        assert_eq!(
            store
                .decrypt_slot(profile.profile_id, "TOKEN", recipient.recipient_id, &secret)
                .expect("head decrypt"),
            b"v2"
        );
        assert_eq!(
            store
                .decrypt_slot_in_state(first_head, "TOKEN", recipient.recipient_id, &secret)
                .expect("superseded still decrypts in the rollback window"),
            b"v1"
        );
    }

    #[test]
    fn metadata_lists_without_decrypting() {
        let (_temp, store) = open_store();
        let signer = signer();
        let (recipient, _secret) = store
            .create_software_recipient(&signer, 1)
            .expect("recipient");
        store
            .create_profile(
                "ci",
                vec![
                    SlotWrite {
                        name: "A".to_string(),
                        value: b"1".to_vec(),
                    },
                    SlotWrite {
                        name: "B".to_string(),
                        value: b"2".to_vec(),
                    },
                ],
                recipient.recipient_id,
                attribution(),
                &signer,
            )
            .expect("create");
        let listed = store.list_profiles().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "ci");
        assert_eq!(listed[0].facet, FacetKind::ConfidentialRuntime);
        assert_eq!(listed[0].slot_names, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(listed[0].lifecycle, LifecycleStatus::Active);
    }

    #[test]
    fn facet_cannot_be_selected_as_source_history() {
        assert!(confidential_runtime_source_history_laws().is_none());
        assert!(!FacetKind::ConfidentialRuntime.git_projection_visits());
        assert!(!FacetKind::ConfidentialRuntime.may_checkout());
        assert!(!FacetKind::ConfidentialRuntime.may_land());
        assert!(
            FacetKind::ConfidentialRuntime
                .require_git_projection()
                .is_err()
        );
        assert!(
            FacetKind::ConfidentialRuntime
                .require_worktree_materialization()
                .is_err()
        );
        assert!(FacetKind::ConfidentialRuntime.require_land().is_err());

        let runtime_id = EnvProfileVersionId::from_bytes([7; 32]);
        let source_id = StateId::from_bytes([7; 32]);
        assert_eq!(runtime_id.as_bytes(), source_id.as_bytes());
        let _need_explicit_conversion = StateId::from_bytes(*runtime_id.as_bytes());
    }

    #[test]
    fn store_is_not_a_source_thread_namespace() {
        let (_temp, store) = open_store();
        let signer = signer();
        let (recipient, _) = store
            .create_software_recipient(&signer, 1)
            .expect("recipient");
        store
            .create_profile(
                "production",
                vec![SlotWrite {
                    name: "X".to_string(),
                    value: b"y".to_vec(),
                }],
                recipient.recipient_id,
                attribution(),
                &signer,
            )
            .expect("create");
        assert!(store.root().ends_with("env"));
        assert!(!store.root().join("refs").exists());
        assert!(RESERVED_MATERIALIZATION_PATHS.contains(&".env"));
        assert!(is_reserved_materialization_path(".env"));
        assert!(is_reserved_materialization_path("src/../.env.local"));
        assert!(!is_reserved_materialization_path("README.md"));
    }

    #[test]
    fn broker_run_injects_values_and_audits_without_plaintext() {
        let plaintext = b"super-secret-value";
        let (temp, broker, signer, _id, _secret) = seeded_broker(
            "production",
            vec![SlotWrite {
                name: "DATABASE_URL".to_string(),
                value: plaintext.to_vec(),
            }],
        );
        let grant = broker
            .authorize(
                &run_request("production", &["DATABASE_URL"], future()),
                &signer,
            )
            .expect("authorize");
        let secrets = broker.unwrap_for_run(grant, &signer).expect("unwrap");
        let pairs = secrets.into_env_pairs().expect("utf8");
        assert_eq!(
            pairs,
            vec![("DATABASE_URL".to_string(), "super-secret-value".to_string())]
        );

        let audit = broker.store().list_audit().expect("audit");
        assert!(
            audit
                .iter()
                .any(|record| record.event == AuditEventKind::Granted)
        );
        assert!(
            audit
                .iter()
                .any(|record| record.event == AuditEventKind::Run)
        );
        for record in &audit {
            crypto::verify_payload_signature(
                &crate::codec::audit_signing_payload(record).expect("payload"),
                &record.signature.algorithm,
                &record.signature.public_key,
                &record.signature.signature,
            )
            .expect("audit signature");
            let bytes = rmp_serde::to_vec_named(record).expect("encode audit");
            assert!(
                !bytes.windows(plaintext.len()).any(|w| w == plaintext),
                "plaintext leaked into audit record"
            );
        }
        for entry in walkdir(temp.path()) {
            let bytes = std::fs::read(&entry).expect("read");
            assert!(
                !bytes.windows(plaintext.len()).any(|w| w == plaintext),
                "plaintext leaked into {}",
                entry.display()
            );
        }
    }

    #[test]
    fn broker_refuses_expired_and_wrong_profile_or_slot() {
        let (_temp, broker, signer, _id, _secret) = seeded_broker(
            "production",
            vec![SlotWrite {
                name: "TOKEN".to_string(),
                value: b"abc".to_vec(),
            }],
        );
        broker
            .authorize(&run_request("production", &["TOKEN"], 1), &signer)
            .expect_err("expired");
        broker
            .authorize(&run_request("missing", &["TOKEN"], future()), &signer)
            .expect_err("wrong profile");
        broker
            .authorize(&run_request("production", &["OTHER"], future()), &signer)
            .expect_err("wrong slot");

        let denied = broker
            .store()
            .list_audit()
            .expect("audit")
            .into_iter()
            .filter(|record| record.event == AuditEventKind::Denied)
            .count();
        assert!(denied >= 3, "expected denied audit rows, got {denied}");
    }

    #[test]
    fn broker_refuses_grant_reuse_and_missing_handle() {
        let (_temp, store) = open_store();
        let signer = signer();
        let (recipient, secret) = store
            .create_software_recipient(&signer, 1)
            .expect("recipient");
        store
            .create_profile(
                "production",
                vec![SlotWrite {
                    name: "TOKEN".to_string(),
                    value: b"abc".to_vec(),
                }],
                recipient.recipient_id,
                attribution(),
                &signer,
            )
            .expect("create");
        let empty = PolicyBroker::new(store, attribution());
        empty
            .authorize(&run_request("production", &["TOKEN"], future()), &signer)
            .expect_err("no handle");

        let (_temp, broker, signer, _id, _held) = seeded_broker(
            "production",
            vec![SlotWrite {
                name: "TOKEN".to_string(),
                value: b"abc".to_vec(),
            }],
        );
        let grant = broker
            .authorize(&run_request("production", &["TOKEN"], future()), &signer)
            .expect("authorize");
        let replay = grant.clone();
        drop(broker.unwrap_for_run(grant, &signer).expect("first unwrap"));
        assert!(
            broker.unwrap_for_run(replay, &signer).is_err(),
            "grant is single-use"
        );
        let _ = secret;
    }

    #[test]
    fn reserved_materialization_paths_are_detected_on_disk() {
        let temp = TempDir::new().expect("tempdir");
        assert!(reserved_materialization_on_disk(temp.path()).is_none());
        std::fs::write(temp.path().join(".env"), b"LEAK=1").expect("write");
        assert_eq!(
            reserved_materialization_on_disk(temp.path()),
            Some(".env")
        );
    }

    #[cfg(unix)]
    #[test]
    fn ipc_run_unwrap_returns_values_and_denies_expired() {
        let (_temp, broker, signer, _id, _secret) = seeded_broker(
            "production",
            vec![SlotWrite {
                name: "TOKEN".to_string(),
                value: b"from-ipc".to_vec(),
            }],
        );
        let listener = bind_broker_socket(broker.store().root()).expect("bind");
        let socket = broker_socket_path(broker.store().root());
        std::thread::scope(|scope| {
            scope.spawn(|| {
                serve_once(&listener, &broker, &signer).expect("serve");
            });
            let pairs = request_run_unwrap(
                &socket,
                &run_request("production", &["TOKEN"], future()),
            )
            .expect("ipc unwrap");
            assert_eq!(pairs, vec![("TOKEN".to_string(), "from-ipc".to_string())]);
        });

        let listener = bind_broker_socket(broker.store().root()).expect("rebind");
        std::thread::scope(|scope| {
            scope.spawn(|| {
                serve_once(&listener, &broker, &signer).expect("serve expired");
            });
            request_run_unwrap(&socket, &run_request("production", &["TOKEN"], 1))
                .expect_err("expired ipc");
        });
    }

    #[test]
    fn signed_lifecycle_is_authoritative_over_an_edited_version_field() {
        // The HIGH finding: flipping the unsigned `lifecycle` field in a
        // version file must NOT re-enable decrypt — the signed record wins.
        let (_temp, store) = open_store();
        let signer = signer();
        let (recipient, secret) = store
            .create_software_recipient(&signer, 1)
            .expect("recipient");
        let profile = store
            .create_profile(
                "p",
                vec![SlotWrite {
                    name: "TOKEN".to_string(),
                    value: b"v".to_vec(),
                }],
                recipient.recipient_id,
                attribution(),
                &signer,
            )
            .expect("create");
        store
            .revoke(profile.profile_id, attribution(), &signer)
            .expect("revoke");
        let head = store.load_profile(profile.profile_id).expect("profile").head;
        // Attacker rewrites the version file's lifecycle back to Active. The
        // state id excludes lifecycle, so the file still "decodes".
        let mut state = store.load_state(head).expect("state");
        state.lifecycle = LifecycleStatus::Active;
        std::fs::write(
            store
                .root()
                .join("versions")
                .join(format!("{}.msgpack", head.to_hex())),
            crate::codec::encode_state(&state).expect("encode"),
        )
        .expect("rewrite");
        let err = store
            .decrypt_slot(profile.profile_id, "TOKEN", recipient.recipient_id, &secret)
            .expect_err("a revoked version must not decrypt despite an edited field");
        assert!(matches!(err, EnvStoreError::DecryptForbidden(_)), "{err:?}");
    }

    #[test]
    fn a_tampered_lifecycle_record_is_rejected_on_read() {
        let (_temp, store) = open_store();
        let signer = signer();
        let (recipient, secret) = store
            .create_software_recipient(&signer, 1)
            .expect("recipient");
        let profile = store
            .create_profile(
                "p",
                vec![SlotWrite {
                    name: "TOKEN".to_string(),
                    value: b"v".to_vec(),
                }],
                recipient.recipient_id,
                attribution(),
                &signer,
            )
            .expect("create");
        let dir = store.root().join("lifecycle");
        let entry = std::fs::read_dir(&dir)
            .expect("read_dir")
            .next()
            .expect("a record")
            .expect("entry")
            .path();
        let mut record = crate::codec::decode_lifecycle(&std::fs::read(&entry).expect("read"))
            .expect("decode");
        record.to = LifecycleStatus::Purged; // signature no longer covers `to`
        std::fs::write(&entry, crate::codec::encode_lifecycle(&record).expect("encode"))
            .expect("rewrite");
        assert!(
            store
                .decrypt_slot(profile.profile_id, "TOKEN", recipient.recipient_id, &secret)
                .is_err(),
            "a tampered lifecycle record must fail the read closed"
        );
        assert!(store.list_lifecycle(profile.profile_id).is_err());
    }

    #[test]
    fn multi_recipient_profile_decrypts_via_each_recipient() {
        let (_temp, store) = open_store();
        let signer = signer();
        let (r1, s1) = store.create_software_recipient(&signer, 1).expect("r1");
        let (r2, s2) = store.create_software_recipient(&signer, 1).expect("r2");
        let profile = store
            .create_profile_with_recipients(
                "p",
                vec![SlotWrite {
                    name: "TOKEN".to_string(),
                    value: b"multi".to_vec(),
                }],
                &[r1.recipient_id, r2.recipient_id],
                attribution(),
                &signer,
            )
            .expect("create");
        assert_eq!(
            store
                .decrypt_slot(profile.profile_id, "TOKEN", r1.recipient_id, &s1)
                .expect("r1 decrypt"),
            b"multi"
        );
        // The second recipient — must NOT depend on wrap ordering.
        assert_eq!(
            store
                .decrypt_slot(profile.profile_id, "TOKEN", r2.recipient_id, &s2)
                .expect("r2 decrypt"),
            b"multi"
        );
        assert!(
            store
                .decrypt_slot(profile.profile_id, "TOKEN", r2.recipient_id, &s1)
                .is_err(),
            "presenting r2's id with r1's key must fail"
        );
    }

    #[test]
    fn revoke_refuses_head_and_superseded_history() {
        let (_temp, store) = open_store();
        let signer = signer();
        let (recipient, secret) = store
            .create_software_recipient(&signer, 1)
            .expect("recipient");
        let profile = store
            .create_profile(
                "p",
                vec![SlotWrite {
                    name: "TOKEN".to_string(),
                    value: b"v1".to_vec(),
                }],
                recipient.recipient_id,
                attribution(),
                &signer,
            )
            .expect("create");
        let first_head = profile.head;
        store
            .update_slots(
                profile.profile_id,
                vec![SlotWrite {
                    name: "TOKEN".to_string(),
                    value: b"v2".to_vec(),
                }],
                attribution(),
                &signer,
            )
            .expect("update");
        assert_eq!(
            store
                .decrypt_slot_in_state(first_head, "TOKEN", recipient.recipient_id, &secret)
                .expect("superseded decrypts before revoke"),
            b"v1"
        );
        store
            .revoke(profile.profile_id, attribution(), &signer)
            .expect("revoke");
        assert!(
            store
                .decrypt_slot(profile.profile_id, "TOKEN", recipient.recipient_id, &secret)
                .is_err(),
            "revoked head must not decrypt"
        );
        assert!(
            store
                .decrypt_slot_in_state(first_head, "TOKEN", recipient.recipient_id, &secret)
                .is_err(),
            "revoke must cover superseded history"
        );
    }

    #[test]
    fn revoke_skips_an_orphan_version_instead_of_aborting() {
        // A version file left with no signed lifecycle records (crash between
        // write_version and its first record) must not brick revocation of the
        // whole profile. revoke skips it (it is already undecryptable) and
        // still revokes the live head.
        let (_temp, store) = open_store();
        let signer = signer();
        let (recipient, secret) = store
            .create_software_recipient(&signer, 1)
            .expect("recipient");
        let profile = store
            .create_profile(
                "p",
                vec![SlotWrite {
                    name: "TOKEN".to_string(),
                    value: b"v1".to_vec(),
                }],
                recipient.recipient_id,
                attribution(),
                &signer,
            )
            .expect("create");
        let orphan = profile.head;
        store
            .update_slots(
                profile.profile_id,
                vec![SlotWrite {
                    name: "TOKEN".to_string(),
                    value: b"v2".to_vec(),
                }],
                attribution(),
                &signer,
            )
            .expect("update");
        // Delete every lifecycle record for the first (now superseded) version,
        // leaving its version file orphaned on disk.
        for entry in std::fs::read_dir(store.root().join("lifecycle")).expect("read_dir") {
            let path = entry.expect("entry").path();
            let record =
                crate::codec::decode_lifecycle(&std::fs::read(&path).expect("read")).expect("decode");
            if record.state_id == orphan {
                std::fs::remove_file(&path).expect("remove");
            }
        }
        // revoke must succeed (not abort on the orphan) and the live head must
        // stop decrypting.
        store
            .revoke(profile.profile_id, attribution(), &signer)
            .expect("revoke must not abort on an orphan version");
        assert!(
            store
                .decrypt_slot(profile.profile_id, "TOKEN", recipient.recipient_id, &secret)
                .is_err(),
            "revoked head must not decrypt"
        );
    }

    #[test]
    fn a_second_signer_cannot_write_to_a_pinned_store() {
        let (_temp, store) = open_store();
        let first = signer();
        store
            .create_software_recipient(&first, 1)
            .expect("first signer pins the identity");
        let foreign = signer();
        match store.create_software_recipient(&foreign, 1) {
            Err(EnvStoreError::Invalid(_)) => {}
            Ok(_) => panic!("a foreign signer must be rejected against the pinned identity"),
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        fn rec(path: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(path).expect("read_dir") {
                let entry = entry.expect("entry");
                let path = entry.path();
                if path.is_dir() {
                    rec(&path, files);
                } else {
                    files.push(path);
                }
            }
        }
        rec(root, &mut files);
        files
    }
}
