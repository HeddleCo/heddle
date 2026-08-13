// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;

use crate::{RecoveryError, Result, StateDocument};

pub(crate) fn validate_documents(documents: &[StateDocument]) -> Result<()> {
    let mut seen = BTreeSet::new();
    if documents.is_empty()
        || documents.iter().any(|document| {
            document.thread.is_empty()
                || document.text.trim().is_empty()
                || !seen.insert(document.state)
        })
    {
        return Err(RecoveryError::InvalidInput(
            "documents need unique states, non-empty threads, and non-empty text".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn corpus_digest(documents: &[StateDocument]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"heddle.semantic-recovery.corpus.v1\0");
    for document in documents {
        hasher.update(&document.state.0);
        hasher.update(&(document.thread.len() as u64).to_be_bytes());
        hasher.update(document.thread.as_bytes());
        hasher.update(&(document.text.len() as u64).to_be_bytes());
        hasher.update(document.text.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

pub(crate) fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}
