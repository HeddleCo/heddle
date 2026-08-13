// SPDX-License-Identifier: Apache-2.0
//! Residual-quantized state index and thread reconstruction query.

use std::{collections::BTreeSet, fmt, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    Embedder, ModelIdentity, RecoveryError, ResidualQuantizerConfig, Result, StateDocument,
    document::{embed_documents, normalize},
    index_support::{corpus_digest, dot, validate_documents},
    quantizer::{ResidualQuantizer, VectorCode, code_bits, theoretical_bits_per_vector},
    storage::{pack_codes, read_sidecar, unpack_codes, write_sidecar},
};

/// Stable 32-byte state identity independent of the owning object crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StateKey(pub [u8; 32]);

impl fmt::Display for StateKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0[..8] {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// One quantized nearest-neighbor result.
#[derive(Clone, Debug, PartialEq)]
pub struct Neighbor {
    /// Neighbor state.
    pub state: StateKey,
    /// Thread metadata carried by the neighbor.
    pub thread: String,
    /// Cosine similarity against the query vector.
    pub similarity: f32,
}

/// Thread inferred exclusively from neighboring states.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadReconstruction {
    /// Predicted thread name.
    pub thread: String,
    /// Similarity of the strongest sibling evidence.
    pub confidence: f32,
    /// Nearest sibling states from the predicted thread.
    pub siblings: Vec<Neighbor>,
}

/// Measurable properties of a newly built sidecar.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexBuildReport {
    /// Indexed state count.
    pub states: usize,
    /// Distinct thread count.
    pub threads: usize,
    /// Ideal residual code information content.
    pub theoretical_bits_per_vector: f64,
    /// Actual packed code width.
    pub packed_bits_per_vector: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    state: StateKey,
    thread: String,
}

#[derive(Serialize, Deserialize)]
struct PersistedIndex {
    model: ModelIdentity,
    corpus_digest: [u8; 32],
    quantizer: ResidualQuantizer,
    entries: Vec<Entry>,
    codes: Vec<u8>,
}

/// Rebuildable residual-quantized index over state/thread documents.
pub struct RecoveryIndex {
    model: ModelIdentity,
    corpus_digest: [u8; 32],
    quantizer: ResidualQuantizer,
    entries: Vec<Entry>,
    codes: Vec<VectorCode>,
    decoded: Vec<Vec<f32>>,
}

impl RecoveryIndex {
    /// Embed, quantize, and index documents with the measured 32+16 default.
    pub fn build<E: Embedder>(
        documents: &[StateDocument],
        embedder: &mut E,
        config: ResidualQuantizerConfig,
    ) -> Result<(Self, IndexBuildReport)> {
        let mut sorted = documents.to_vec();
        sorted.sort_by_key(|document| document.state);
        validate_documents(&sorted)?;
        let (model, vectors) = embed_documents(embedder, &sorted)?;
        Self::build_from_embeddings(&sorted, model, vectors, config)
    }

    /// Build from precomputed normalized vectors, used by reproducible sweeps.
    pub fn build_from_embeddings(
        documents: &[StateDocument],
        model: ModelIdentity,
        mut vectors: Vec<Vec<f32>>,
        config: ResidualQuantizerConfig,
    ) -> Result<(Self, IndexBuildReport)> {
        validate_documents(documents)?;
        if documents.len() != vectors.len() {
            return Err(RecoveryError::InvalidInput(format!(
                "{} documents have {} vectors",
                documents.len(),
                vectors.len()
            )));
        }
        for vector in &mut vectors {
            normalize(vector)?;
        }
        if vectors
            .iter()
            .any(|vector| vector.len() != model.dimensions)
        {
            return Err(RecoveryError::InvalidInput(format!(
                "model declares {} dimensions but vectors disagree",
                model.dimensions
            )));
        }
        let quantizer = ResidualQuantizer::train(&vectors, config)?;
        let codes = vectors
            .iter()
            .map(|vector| quantizer.encode(vector))
            .collect::<Result<Vec<_>>>()?;
        let decoded = codes
            .iter()
            .map(|code| quantizer.decode(*code))
            .collect::<Result<Vec<_>>>()?;
        let entries = documents
            .iter()
            .map(|document| Entry {
                state: document.state,
                thread: document.thread.clone(),
            })
            .collect();
        let report = IndexBuildReport {
            states: documents.len(),
            threads: documents
                .iter()
                .map(|document| document.thread.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            theoretical_bits_per_vector: theoretical_bits_per_vector(config),
            packed_bits_per_vector: quantizer.packed_bits(),
        };
        Ok((
            Self {
                model,
                corpus_digest: corpus_digest(documents),
                quantizer,
                entries,
                codes,
                decoded,
            },
            report,
        ))
    }

    /// Return quantized neighbors, excluding an optional query state.
    pub fn search(
        &self,
        query: &[f32],
        limit: usize,
        exclude: Option<StateKey>,
    ) -> Result<Vec<Neighbor>> {
        if query.len() != self.model.dimensions || query.iter().any(|value| !value.is_finite()) {
            return Err(RecoveryError::InvalidInput(format!(
                "query must contain {} finite values",
                self.model.dimensions
            )));
        }
        let mut query = query.to_vec();
        normalize(&mut query)?;
        let mut neighbors: Vec<Neighbor> = self
            .entries
            .iter()
            .zip(&self.decoded)
            .filter(|(entry, _)| Some(entry.state) != exclude)
            .map(|(entry, vector)| Neighbor {
                state: entry.state,
                thread: entry.thread.clone(),
                similarity: dot(&query, vector),
            })
            .collect();
        neighbors.sort_by(|left, right| {
            right
                .similarity
                .total_cmp(&left.similarity)
                .then_with(|| left.state.cmp(&right.state))
        });
        neighbors.truncate(limit);
        Ok(neighbors)
    }

    /// Infer a state's thread from its nearest sibling evidence.
    pub fn reconstruct_thread(
        &self,
        state: StateKey,
        query: &[f32],
        sibling_limit: usize,
    ) -> Result<Option<ThreadReconstruction>> {
        if sibling_limit == 0 || !self.entries.iter().any(|entry| entry.state == state) {
            return Ok(None);
        }
        let neighbors = self.search(query, self.entries.len().saturating_sub(1), Some(state))?;
        let Some(strongest) = neighbors.first() else {
            return Ok(None);
        };
        let thread = strongest.thread.clone();
        let confidence = strongest.similarity;
        let siblings = neighbors
            .into_iter()
            .filter(|neighbor| neighbor.thread == thread)
            .take(sibling_limit)
            .collect();
        Ok(Some(ThreadReconstruction {
            thread,
            confidence,
            siblings,
        }))
    }

    /// Persist this index atomically and return its complete byte size.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<u64> {
        let coarse_bits = code_bits(self.quantizer.config.coarse_centroids);
        let residual_bits = code_bits(self.quantizer.config.residual_centroids);
        let persisted = PersistedIndex {
            model: self.model.clone(),
            corpus_digest: self.corpus_digest,
            quantizer: self.quantizer.clone(),
            entries: self.entries.clone(),
            codes: pack_codes(&self.codes, coarse_bits, residual_bits),
        };
        write_sidecar(path.as_ref(), &persisted)
    }

    /// Load and validate a recovery sidecar.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let persisted: PersistedIndex = read_sidecar(path.as_ref())?;
        persisted.quantizer.validate()?;
        if persisted.model.dimensions != persisted.quantizer.dimensions {
            return Err(RecoveryError::InvalidSidecar(
                "model and quantizer dimensions disagree".to_string(),
            ));
        }
        let coarse_bits = code_bits(persisted.quantizer.config.coarse_centroids);
        let residual_bits = code_bits(persisted.quantizer.config.residual_centroids);
        let codes = unpack_codes(
            &persisted.codes,
            persisted.entries.len(),
            coarse_bits,
            residual_bits,
        )?;
        let decoded = codes
            .iter()
            .map(|code| persisted.quantizer.decode(*code))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            model: persisted.model,
            corpus_digest: persisted.corpus_digest,
            quantizer: persisted.quantizer,
            entries: persisted.entries,
            codes,
            decoded,
        })
    }

    /// Model identity recorded by this index.
    pub fn model(&self) -> &ModelIdentity {
        &self.model
    }

    /// Digest of the input document corpus at rebuild time.
    pub fn corpus_digest(&self) -> [u8; 32] {
        self.corpus_digest
    }
}
