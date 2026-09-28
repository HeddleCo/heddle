// SPDX-License-Identifier: Apache-2.0
//! The exact local briefing inputs attached to the next capture.

use std::fs;

use crypto::{public_key_bytes, verify_payload_signature};
use objects::{
    error::HeddleError,
    fs_atomic::write_file_atomic,
    object::{Blob, StateAttachment, StateAttachmentBody, StateId},
    store::ObjectStore,
};
use serde::{Deserialize, Serialize};

use crate::{Repository, Result, StateAttachmentKind};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuppliedAnnotation {
    pub target: String,
    pub annotation_id: String,
    pub revisions: Vec<SuppliedRevision>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuppliedRevision {
    pub revision_id: String,
    pub kind: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextConsumptionReceipt {
    pub format_version: u8,
    pub thread: String,
    pub intent_versions: Vec<String>,
    pub annotations: Vec<SuppliedAnnotation>,
    pub briefing_hash: String,
}

#[derive(Serialize, Deserialize)]
struct SignedContextReceipt {
    state_id: StateId,
    receipt: ContextConsumptionReceipt,
    algorithm: String,
    public_key: Vec<u8>,
    signature: Vec<u8>,
}

impl Repository {
    pub fn ensure_context_receipt_signer(&self) -> Result<()> {
        if self.signing_signer().is_none() {
            return Err(HeddleError::InvalidObject(
                "a context briefing requires a signing identity before capture".into(),
            ));
        }
        Ok(())
    }

    fn pending_context_receipt_path(&self) -> std::path::PathBuf {
        self.heddle_dir()
            .join(format!("context-briefing-{}.json", self.op_scope()))
    }

    pub fn write_pending_context_receipt(&self, receipt: &ContextConsumptionReceipt) -> Result<()> {
        validate(receipt)?;
        let path = self.pending_context_receipt_path();
        let bytes = serde_json::to_vec(receipt).map_err(|error| {
            HeddleError::InvalidObject(format!("encode context receipt: {error}"))
        })?;
        write_file_atomic(&path, &bytes)?;
        Ok(())
    }

    pub fn pending_context_receipt(&self) -> Result<Option<ContextConsumptionReceipt>> {
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
        Ok(Some(receipt))
    }

    pub fn attach_context_receipt(
        &self,
        state_id: StateId,
        receipt: &ContextConsumptionReceipt,
    ) -> Result<()> {
        validate(receipt)?;
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
    let mut payload = b"heddle-context-consumption-v1\0".to_vec();
    payload.extend_from_slice(state_id.as_bytes());
    payload.extend(
        serde_json::to_vec(receipt).map_err(|error| {
            HeddleError::InvalidObject(format!("encode context receipt: {error}"))
        })?,
    );
    Ok(payload)
}

fn validate(receipt: &ContextConsumptionReceipt) -> Result<()> {
    if receipt.format_version != 1
        || receipt.thread.is_empty()
        || receipt.thread.len() > 1024
        || receipt.briefing_hash.is_empty()
        || receipt.annotations.len() > 64
        || receipt.annotations.iter().any(|annotation| {
            annotation.annotation_id.is_empty()
                || annotation.revisions.is_empty()
                || annotation.revisions.len() > 64
                || annotation.revisions.iter().any(|revision| {
                    revision.revision_id.is_empty()
                        || revision.kind.is_empty()
                        || revision.content.is_empty()
                })
        })
    {
        return Err(HeddleError::InvalidObject("invalid context receipt".into()));
    }
    Ok(())
}
