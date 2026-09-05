// SPDX-License-Identifier: Apache-2.0
//! Local confidential-runtime profile store (ADR 0051 / heddle#999).
//!
//! This crate is the library MVP: typed roots, immutable versions, recipient
//! wrapping, and signed lifecycle records. Decrypt that accepts a software
//! recipient secret is the provider unwrap boundary. The policy broker, daemon
//! IPC, and `heddle env run` are later slices. There is no CLI here on
//! purpose — a command that held the software key would pretend to be secure.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod codec;
mod error;
mod ids;
mod store;
mod types;

pub use error::{Result, RuntimeProfileError};
pub use ids::{
    CiphertextId, LifecycleRecordId, RecipientId, RuntimeProfileId, RuntimeProfileStateId,
};
pub use store::{RuntimeProfileStore, SlotWrite, confidential_runtime_source_history_laws};
pub use types::{
    FacetKindWire, LifecycleRecord, LifecycleStatus, ProfileMetadata, ProviderCapability,
    RESERVED_MATERIALIZATION_PATHS, RUNTIME_PROFILE_SCHEMA_VERSION, RecipientDescriptor,
    RuntimeProfileRef, RuntimeProfileState, SignatureBlock, SlotMetadata, SlotRecord,
    WrappedDekRecord,
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

    fn open_store() -> (TempDir, RuntimeProfileStore) {
        let temp = TempDir::new().expect("tempdir");
        let store = RuntimeProfileStore::open(temp.path()).expect("open store");
        (temp, store)
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
            .decrypt_slot(profile.profile_id, "DATABASE_URL", &secret)
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
            .decrypt_slot(profile.profile_id, "API_TOKEN", &stranger)
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
                .decrypt_slot(profile.profile_id, "TOKEN", &secret)
                .expect("head decrypt"),
            b"v2"
        );
        assert_eq!(
            store
                .decrypt_slot_in_state(first_head, "TOKEN", &secret)
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

        // Distinct identity: a runtime-profile state id is 32 bytes but is not
        // a Source History StateId at the type level. This assignment would
        // fail to compile if the types were aliased:
        let runtime_id = RuntimeProfileStateId::from_bytes([7; 32]);
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
        assert!(store.root().ends_with("runtime-profiles"));
        assert!(!store.root().join("refs").exists());
        assert!(RESERVED_MATERIALIZATION_PATHS.contains(&".env"));
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
