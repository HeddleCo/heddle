// SPDX-License-Identifier: Apache-2.0
//! The exact local briefing inputs attached to the next capture.

use std::{fs, io::Write};

use crypto::{Ed25519Signer, Signer, public_key_bytes, verify_payload_signature};
use objects::{
    error::HeddleError,
    fs_atomic::write_file_atomic,
    object::{Blob, ContentHash, StateAttachment, StateAttachmentBody, StateId},
    store::ObjectStore,
};
use serde::{Deserialize, Serialize};

use crate::{Repository, Result, StateAttachmentKind};

pub const CONTEXT_RECEIPT_ATTESTATION: &str = "local_self_attested";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuppliedAnnotation {
    pub target: String,
    pub annotation_id: String,
    pub visibility: String,
    pub revisions: Vec<SuppliedRevision>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuppliedRevision {
    pub revision_id: String,
    pub kind: String,
    pub content_hash: String,
}

/// A self-attested local record: proves what this repository's key signed,
/// not independent delivery to the recipient. A same-user process can sign
/// another claim with the repository key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextConsumptionReceipt {
    pub format_version: u8,
    pub attestation: String,
    pub thread: String,
    pub recipient_lane: String,
    pub nonce: String,
    pub intent_versions: Vec<String>,
    pub annotations: Vec<SuppliedAnnotation>,
    pub briefing_hash: String,
    pub supplier_algorithm: String,
    pub supplier_public_key: Vec<u8>,
    pub supplier_signature: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct SignedContextReceipt {
    // The supplier signature lives inside `receipt`; this second signature
    // binds its digest to the capture so an attachment cannot be replayed on
    // another State.
    state_id: StateId,
    receipt: ContextConsumptionReceipt,
    algorithm: String,
    public_key: Vec<u8>,
    signature: Vec<u8>,
}

impl Repository {
    pub fn ensure_context_receipt_signer(&self) -> Result<()> {
        let _serialization = self.installation_lock()?;
        if self.signing_signer().is_none() {
            return Err(HeddleError::InvalidObject(
                "a supplied briefing requires a signed capture reference".into(),
            ));
        }
        Ok(())
    }

    fn pending_context_receipt_path(&self) -> std::path::PathBuf {
        self.heddle_dir()
            .join(format!("context-briefing-{}.json", self.op_scope()))
    }

    fn consume_context_receipt_nonce(&self, nonce: &str, state_id: &StateId) -> Result<()> {
        let directory = self.heddle_dir().join("context-consumed-nonces");
        fs::create_dir_all(&directory)?;
        let digest = ContentHash::compute_typed("context-receipt-nonce", nonce.as_bytes());
        let path = directory.join(format!("{}.nonce", digest.to_hex()));
        let mut marker = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(marker) => marker,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(HeddleError::InvalidObject(
                    "context briefing nonce was already consumed".into(),
                ));
            }
            Err(error) => return Err(error.into()),
        };
        marker.write_all(state_id.to_string_full().as_bytes())?;
        marker.sync_all()?;
        #[cfg(unix)]
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    }

    pub fn write_pending_context_receipt(&self, receipt: &ContextConsumptionReceipt) -> Result<()> {
        let _serialization = self.installation_lock()?;
        validate(receipt)?;
        if self.current_lane()?.as_deref() != Some(receipt.recipient_lane.as_str())
            || receipt.thread != receipt.recipient_lane
        {
            return Err(HeddleError::InvalidObject(
                "context briefing recipient differs from current lane".into(),
            ));
        }
        let local = crate::identity::load_or_mint_local(
            &self.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE),
        )?;
        let signer = Ed25519Signer::from_pem(&local.private_key_pem).map_err(|error| {
            HeddleError::InvalidObject(format!("repository briefing key: {error}"))
        })?;
        let mut signed = receipt.clone();
        signed.supplier_algorithm = signer.algorithm().to_owned();
        signed.supplier_public_key = signer.public_key().to_vec();
        signed.supplier_signature = signer.sign(&supply_payload(&signed)?).map_err(|error| {
            HeddleError::InvalidObject(format!("sign supplied briefing: {error}"))
        })?;
        let path = self.pending_context_receipt_path();
        let bytes = serde_json::to_vec(&signed).map_err(|error| {
            HeddleError::InvalidObject(format!("encode context receipt: {error}"))
        })?;
        write_file_atomic(&path, &bytes)?;
        Ok(())
    }

    pub fn pending_context_receipt(&self) -> Result<Option<ContextConsumptionReceipt>> {
        let _serialization = self.installation_lock()?;
        let path = self.pending_context_receipt_path();
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if bytes.len() > 128 * 1024 {
            return Err(HeddleError::InvalidObject(
                "context receipt exceeds bound".into(),
            ));
        }
        let receipt: ContextConsumptionReceipt =
            serde_json::from_slice(&bytes).map_err(|error| {
                HeddleError::InvalidObject(format!("decode context receipt: {error}"))
            })?;
        validate(&receipt)?;
        verify_supply(&receipt)?;
        if self.current_lane()?.as_deref() != Some(receipt.recipient_lane.as_str()) {
            return Err(HeddleError::InvalidObject(
                "context briefing belongs to another lane".into(),
            ));
        }
        let local = crate::identity::load_local_signer(
            &self.heddle_dir().join(crate::identity::LOCAL_IDENTITY_FILE),
        )
        .ok_or_else(|| HeddleError::InvalidObject("briefing supplier key missing".into()))?;
        if local.public_key() != receipt.supplier_public_key {
            return Err(HeddleError::InvalidObject(
                "briefing was not signed by this repository".into(),
            ));
        }
        Ok(Some(receipt))
    }

    pub fn attach_context_receipt(
        &self,
        state_id: StateId,
        receipt: &ContextConsumptionReceipt,
    ) -> Result<()> {
        let _serialization = self.installation_lock()?;
        validate(receipt)?;
        verify_supply(receipt)?;
        if self.current_lane()?.as_deref() != Some(receipt.recipient_lane.as_str()) {
            return Err(HeddleError::InvalidObject(
                "context briefing belongs to another lane".into(),
            ));
        }
        if self
            .latest_state_attachment(&state_id, StateAttachmentKind::ContextConsumption)?
            .is_some()
        {
            return Err(HeddleError::InvalidObject(
                "capture already has a context receipt".into(),
            ));
        }
        let state = self
            .store()
            .get_state(&state_id)?
            .ok_or(HeddleError::StateNotFound(state_id))?;
        let signer = self.signing_signer().ok_or_else(|| {
            HeddleError::InvalidObject("context receipt signing identity unavailable".into())
        })?;
        let state_signature = self
            .get_state_signature(&state_id)?
            .ok_or_else(|| HeddleError::InvalidObject("capture is unsigned".into()))?;
        let signing_key = public_key_bytes(&state_signature)
            .map_err(|error| HeddleError::InvalidObject(format!("capture signing key: {error}")))?;
        if state_signature.algorithm != signer.algorithm() || signing_key != signer.public_key() {
            return Err(HeddleError::InvalidObject(
                "context receipt signer differs from capture signer".into(),
            ));
        }
        let payload = receipt_payload(&state_id, receipt)?;
        let signature = signer.sign(&payload).map_err(|error| {
            HeddleError::InvalidObject(format!("sign context receipt: {error}"))
        })?;
        let signed = SignedContextReceipt {
            state_id,
            receipt: receipt.clone(),
            algorithm: signer.algorithm().to_owned(),
            public_key: signer.public_key().to_vec(),
            signature,
        };
        let bytes = serde_json::to_vec(&signed).map_err(|error| {
            HeddleError::InvalidObject(format!("encode context receipt: {error}"))
        })?;
        // Claim before publishing the attachment. A failed later write leaves
        // the nonce spent, so a restored pending file cannot replay it.
        self.consume_context_receipt_nonce(&receipt.nonce, &state_id)?;
        let hash = self.store().put_blob(&Blob::new(bytes))?;
        self.put_state_attachment(&StateAttachment {
            state_id,
            body: StateAttachmentBody::ContextConsumption(hash),
            attribution: state.attribution,
            created_at: chrono::Utc::now(),
            supersedes: None,
        })?;
        fs::remove_file(self.pending_context_receipt_path())?;
        Ok(())
    }

    pub fn context_receipt(&self, state_id: &StateId) -> Result<Option<ContextConsumptionReceipt>> {
        let mut attachments = self
            .list_state_attachments(state_id)?
            .into_iter()
            .filter(|attachment| attachment.body.kind() == StateAttachmentKind::ContextConsumption);
        let Some(attachment) = attachments.next() else {
            return Ok(None);
        };
        if attachments.next().is_some() {
            return Err(HeddleError::InvalidObject(
                "capture has conflicting context receipts".into(),
            ));
        }
        let StateAttachmentBody::ContextConsumption(hash) = attachment.body else {
            return Err(HeddleError::InvalidObject(
                "context receipt attachment kind mismatch".into(),
            ));
        };
        let blob = self
            .store()
            .get_blob(&hash)?
            .ok_or_else(|| HeddleError::InvalidObject("context receipt blob missing".into()))?;
        let signed: SignedContextReceipt =
            serde_json::from_slice(blob.content()).map_err(|error| {
                HeddleError::InvalidObject(format!("decode context receipt: {error}"))
            })?;
        validate(&signed.receipt)?;
        verify_supply(&signed.receipt)?;
        if signed.state_id != *state_id {
            return Err(HeddleError::InvalidObject(
                "context receipt belongs to another capture".into(),
            ));
        }
        let state_signature = self
            .get_state_signature(state_id)?
            .ok_or_else(|| HeddleError::InvalidObject("capture signature missing".into()))?;
        let signing_key = public_key_bytes(&state_signature)
            .map_err(|error| HeddleError::InvalidObject(format!("capture signing key: {error}")))?;
        if signed.algorithm != state_signature.algorithm || signed.public_key != signing_key {
            return Err(HeddleError::InvalidObject(
                "context receipt signer differs from capture signer".into(),
            ));
        }
        let payload = receipt_payload(state_id, &signed.receipt)?;
        verify_payload_signature(
            &payload,
            &signed.algorithm,
            &signed.public_key,
            &signed.signature,
        )
        .map_err(|error| {
            HeddleError::InvalidObject(format!("context receipt signature: {error}"))
        })?;
        Ok(Some(signed.receipt))
    }
}

fn receipt_payload(state_id: &StateId, receipt: &ContextConsumptionReceipt) -> Result<Vec<u8>> {
    let mut payload = b"heddle-context-consumption-v2\0".to_vec();
    payload.extend_from_slice(state_id.as_bytes());
    payload.extend(
        serde_json::to_vec(receipt).map_err(|error| {
            HeddleError::InvalidObject(format!("encode context receipt: {error}"))
        })?,
    );
    Ok(payload)
}

fn supply_payload(receipt: &ContextConsumptionReceipt) -> Result<Vec<u8>> {
    let mut unsigned = receipt.clone();
    unsigned.supplier_signature.clear();
    let mut payload = b"heddle-context-briefing-supply-v2\0".to_vec();
    payload.extend(serde_json::to_vec(&unsigned).map_err(|error| {
        HeddleError::InvalidObject(format!("encode supplied briefing: {error}"))
    })?);
    Ok(payload)
}

fn verify_supply(receipt: &ContextConsumptionReceipt) -> Result<()> {
    if receipt.supplier_signature.is_empty() {
        return Err(HeddleError::InvalidObject(
            "briefing has no supplier signature".into(),
        ));
    }
    verify_payload_signature(
        &supply_payload(receipt)?,
        &receipt.supplier_algorithm,
        &receipt.supplier_public_key,
        &receipt.supplier_signature,
    )
    .map_err(|error| HeddleError::InvalidObject(format!("briefing supplier signature: {error}")))
}

fn validate(receipt: &ContextConsumptionReceipt) -> Result<()> {
    if receipt.format_version != 2
        || receipt.attestation != CONTEXT_RECEIPT_ATTESTATION
        || receipt.thread.is_empty()
        || receipt.thread.len() > 1024
        || receipt.recipient_lane.is_empty()
        || receipt.recipient_lane.len() > 1024
        || receipt.recipient_lane != receipt.thread
        || receipt.nonce.is_empty()
        || receipt.nonce.len() > 128
        || receipt.briefing_hash.is_empty()
        || receipt.annotations.len() > 64
        || receipt.annotations.iter().any(|annotation| {
            annotation.annotation_id.is_empty()
                || annotation.visibility.is_empty()
                || annotation.revisions.is_empty()
                || annotation.revisions.len() > 64
                || annotation.revisions.iter().any(|revision| {
                    revision.revision_id.is_empty()
                        || revision.kind.is_empty()
                        || revision.content_hash.is_empty()
                })
        })
    {
        return Err(HeddleError::InvalidObject("invalid context receipt".into()));
    }
    Ok(())
}

#[cfg(test)]
mod review_1863 {
    use super::*;
    #[test]
    fn review_unsigned_pending_tamper_must_be_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = crate::init_test_repository(dir.path()).unwrap();
        std::fs::write(dir.path().join("file.txt"), "seed").unwrap();
        let state = repo.snapshot(Some("capture".into()), None).unwrap();
        let original = ContextConsumptionReceipt {
            format_version: 2,
            attestation: CONTEXT_RECEIPT_ATTESTATION.into(),
            thread: "main".into(),
            recipient_lane: "main".into(),
            nonce: uuid::Uuid::now_v7().to_string(),
            intent_versions: vec!["real-version".into()],
            annotations: vec![SuppliedAnnotation {
                target: "file.txt".into(),
                annotation_id: "real-annotation".into(),
                visibility: "private:embargo".into(),
                revisions: vec![SuppliedRevision {
                    revision_id: "real-revision".into(),
                    kind: "constraint".into(),
                    content_hash: objects::object::ContentHash::compute_typed(
                        "context-supplied-revision",
                        b"real constraint",
                    )
                    .to_string(),
                }],
            }],
            briefing_hash: "original-hash".into(),
            supplier_algorithm: String::new(),
            supplier_public_key: Vec::new(),
            supplier_signature: Vec::new(),
        };
        repo.write_pending_context_receipt(&original).unwrap();
        let pending = repo.pending_context_receipt().unwrap().unwrap();
        let mut tampered = pending.clone();
        tampered.annotations[0].revisions[0].content_hash = "never-supplied".into();
        std::fs::write(
            repo.pending_context_receipt_path(),
            serde_json::to_vec(&tampered).unwrap(),
        )
        .unwrap();
        assert!(
            repo.pending_context_receipt().is_err(),
            "editable pending claim verified"
        );
        std::fs::write(
            repo.pending_context_receipt_path(),
            serde_json::to_vec(&pending).unwrap(),
        )
        .unwrap();
        repo.attach_context_receipt(state.state_id, &pending)
            .unwrap();
        let verified = repo.context_receipt(&state.state_id).unwrap().unwrap();
        let attachment = repo
            .latest_state_attachment(&state.state_id, StateAttachmentKind::ContextConsumption)
            .unwrap()
            .unwrap();
        let StateAttachmentBody::ContextConsumption(hash) = attachment.body else {
            panic!("receipt attachment");
        };
        let closure =
            objects::transfer::enumerate_state_closure(repo.store(), state.state_id).unwrap();
        assert!(
            closure
                .iter()
                .any(|object| object.id == objects::transfer::ObjectId::Hash(hash))
        );
        let blob = repo.store().get_blob(&hash).unwrap().unwrap();
        assert!(
            !String::from_utf8_lossy(blob.content()).contains("real constraint"),
            "plaintext entered State transfer"
        );
        std::fs::write(dir.path().join("file.txt"), "second").unwrap();
        let second = repo.snapshot(Some("second capture".into()), None).unwrap();
        let mut replay = attachment.clone();
        replay.state_id = second.state_id;
        repo.put_state_attachment(&replay).unwrap();
        assert!(
            repo.context_receipt(&second.state_id).is_err(),
            "signed cross-State replay is rejected"
        );
        let status_before = format!(
            "{:?}",
            repo.verify_state_signature(&state.state_id).unwrap()
        );
        let attachment_file = repo
            .store()
            .root()
            .join("objects/state-attachments")
            .join(state.state_id.to_string_full())
            .join(format!("{}.attachment", attachment.id().as_hash().to_hex()));
        std::fs::remove_file(attachment_file).unwrap();
        assert!(
            repo.context_receipt(&state.state_id).unwrap().is_none(),
            "stripping receipt is accepted as absence"
        );
        assert_eq!(
            format!(
                "{:?}",
                repo.verify_state_signature(&state.state_id).unwrap()
            ),
            status_before,
            "State signature remains valid after stripping"
        );
        eprintln!(
            "REVIEW: receipt included in State transfer; cross-State replay rejected; receipt stripped without invalidating State signature"
        );
        assert_eq!(
            verified, pending,
            "capture must preserve supplier-signed claims"
        );
    }
}

#[cfg(test)]
mod round3_receipts {
    use super::*;

    fn claim() -> ContextConsumptionReceipt {
        ContextConsumptionReceipt {
            format_version: 2,
            attestation: CONTEXT_RECEIPT_ATTESTATION.into(),
            thread: "main".into(),
            recipient_lane: "main".into(),
            nonce: uuid::Uuid::now_v7().to_string(),
            intent_versions: vec!["intent-v1".into()],
            annotations: vec![],
            briefing_hash: "briefing-digest".into(),
            supplier_algorithm: String::new(),
            supplier_public_key: vec![],
            supplier_signature: vec![],
        }
    }

    #[test]
    fn round3_review_pending_json_has_attestation() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = crate::init_test_repository(dir.path()).unwrap();
        repo.write_pending_context_receipt(&claim()).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(repo.pending_context_receipt_path()).unwrap())
                .unwrap();
        assert_eq!(
            value["attestation"], "local_self_attested",
            "pending serialized receipt must identify its trust model"
        );
    }

    #[test]
    fn round3_review_transfer_receipt_has_attestation() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = crate::init_test_repository(dir.path()).unwrap();
        std::fs::write(dir.path().join("file"), "seed").unwrap();
        let state = repo.snapshot(Some("capture".into()), None).unwrap();
        repo.write_pending_context_receipt(&claim()).unwrap();
        let pending = repo.pending_context_receipt().unwrap().unwrap();
        repo.attach_context_receipt(state.state_id, &pending)
            .unwrap();
        let attachment = repo
            .latest_state_attachment(&state.state_id, StateAttachmentKind::ContextConsumption)
            .unwrap()
            .unwrap();
        let StateAttachmentBody::ContextConsumption(hash) = attachment.body else {
            panic!("receipt")
        };
        let closure =
            objects::transfer::enumerate_state_closure(repo.store(), state.state_id).unwrap();
        assert!(
            closure
                .iter()
                .any(|object| object.id == objects::transfer::ObjectId::Hash(hash))
        );
        let blob = repo.store().get_blob(&hash).unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_slice(blob.content()).unwrap();
        assert_eq!(
            value["receipt"]["attestation"], "local_self_attested",
            "exported signed receipt must identify its trust model"
        );
    }

    #[test]
    fn attestation_is_required_and_signed() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = crate::init_test_repository(dir.path()).unwrap();
        repo.write_pending_context_receipt(&claim()).unwrap();
        let signed = repo.pending_context_receipt().unwrap().unwrap();
        let mut missing = serde_json::to_value(&signed).unwrap();
        missing.as_object_mut().unwrap().remove("attestation");
        assert!(serde_json::from_value::<ContextConsumptionReceipt>(missing).is_err());

        let mut changed = signed.clone();
        changed.attestation = "independently_verified".into();
        assert!(validate(&changed).is_err());
        assert!(verify_supply(&changed).is_err());
        assert!(repo.write_pending_context_receipt(&changed).is_err());
    }

    #[test]
    fn consumed_nonce_cannot_attach_to_another_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = crate::init_test_repository(dir.path()).unwrap();
        std::fs::write(dir.path().join("file"), "first").unwrap();
        let first = repo.snapshot(Some("first".into()), None).unwrap();
        repo.write_pending_context_receipt(&claim()).unwrap();
        let pending = repo.pending_context_receipt().unwrap().unwrap();
        let saved = std::fs::read(repo.pending_context_receipt_path()).unwrap();
        repo.attach_context_receipt(first.state_id, &pending)
            .unwrap();
        std::fs::write(dir.path().join("file"), "second").unwrap();
        let second = repo.snapshot(Some("second".into()), None).unwrap();
        std::fs::write(repo.pending_context_receipt_path(), saved).unwrap();
        let reopened = crate::Repository::open(dir.path()).unwrap();
        let replay = reopened.pending_context_receipt().unwrap().unwrap();
        assert!(
            reopened
                .attach_context_receipt(second.state_id, &replay)
                .is_err()
        );
        assert!(
            reopened
                .context_receipt(&second.state_id)
                .unwrap()
                .is_none()
        );
    }
}
