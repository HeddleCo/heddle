// SPDX-License-Identifier: Apache-2.0
//! State documents and the measured four-chunk embedding policy.

use serde::{Deserialize, Serialize};

use crate::{Embedder, ModelIdentity, RecoveryError, Result, StateKey};

/// Maximum characters in one model input, matching the investigation.
pub const EMBEDDING_CHUNK_CHARS: usize = 800;
/// Maximum evenly spaced chunks embedded for one state.
pub const MAX_EMBEDDING_CHUNKS: usize = 4;

/// Textual recovery input associated with one state and its known thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateDocument {
    /// Immutable Heddle state identity.
    pub state: StateKey,
    /// Ground-truth thread label stored as retrieval metadata.
    pub thread: String,
    /// Intent plus changed paths and bounded changed source content.
    pub text: String,
}

/// Embed state documents using four evenly spaced 800-character chunks.
pub fn embed_documents<E: Embedder>(
    embedder: &mut E,
    documents: &[StateDocument],
) -> Result<(ModelIdentity, Vec<Vec<f32>>)> {
    if documents.is_empty() {
        return Err(RecoveryError::InvalidInput(
            "at least one state document is required".to_string(),
        ));
    }
    let mut chunks = Vec::new();
    let mut ranges = Vec::with_capacity(documents.len());
    for document in documents {
        let start = chunks.len();
        chunks.extend(even_chunks(&document.text));
        if chunks.len() == start {
            return Err(RecoveryError::InvalidInput(format!(
                "state {} has no embeddable text",
                document.state
            )));
        }
        ranges.push(start..chunks.len());
    }

    let identity = embedder.identity();
    let chunk_vectors = embedder.embed(&chunks)?;
    if chunk_vectors.len() != chunks.len() {
        return Err(RecoveryError::Embedding(format!(
            "model returned {} vectors for {} chunks",
            chunk_vectors.len(),
            chunks.len()
        )));
    }
    let vectors = ranges
        .into_iter()
        .map(|range| average_normalized(&chunk_vectors[range], identity.dimensions))
        .collect::<Result<Vec<_>>>()?;
    Ok((identity, vectors))
}

fn even_chunks(text: &str) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    if characters.is_empty() {
        return Vec::new();
    }
    if characters.len() <= EMBEDDING_CHUNK_CHARS {
        return vec![text.to_string()];
    }
    let last_start = characters.len() - EMBEDDING_CHUNK_CHARS;
    (0..MAX_EMBEDDING_CHUNKS)
        .map(|index| {
            let start = index * last_start / (MAX_EMBEDDING_CHUNKS - 1);
            characters[start..start + EMBEDDING_CHUNK_CHARS]
                .iter()
                .collect()
        })
        .collect()
}

fn average_normalized(vectors: &[Vec<f32>], dimensions: usize) -> Result<Vec<f32>> {
    let mut average = vec![0.0_f32; dimensions];
    for vector in vectors {
        if vector.len() != dimensions || vector.iter().any(|value| !value.is_finite()) {
            return Err(RecoveryError::Embedding(format!(
                "model returned an invalid vector; expected {dimensions} finite values"
            )));
        }
        for (total, value) in average.iter_mut().zip(vector) {
            *total += value;
        }
    }
    normalize(&mut average)?;
    Ok(average)
}

pub(crate) fn normalize(vector: &mut [f32]) -> Result<()> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return Err(RecoveryError::InvalidInput(
            "cannot normalize a zero or non-finite vector".to_string(),
        ));
    }
    vector.iter_mut().for_each(|value| *value /= norm);
    Ok(())
}
