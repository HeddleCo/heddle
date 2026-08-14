// SPDX-License-Identifier: Apache-2.0
//! Rebuildable semantic retrieval over Heddle states.
//!
//! The index is deliberately a non-authoritative sidecar. It contains local
//! model output, residual codebooks, and state/thread lookup metadata; it does
//! not participate in object identity, refs, packs, or capture correctness.

mod document;
mod embedding;
mod error;
mod index;
mod index_support;
mod quantizer;
mod storage;

pub use document::{EMBEDDING_CHUNK_CHARS, MAX_EMBEDDING_CHUNKS, StateDocument, embed_documents};
pub use embedding::{
    BGE_SMALL_ARTIFACT_SHA256, BGE_SMALL_DIMENSIONS, BGE_SMALL_MODEL_ID, BgeSmallEmbedder,
    Embedder, ModelIdentity,
};
pub use error::{RecoveryError, Result};
pub use index::{IndexBuildReport, Neighbor, RecoveryIndex, StateKey, ThreadReconstruction};
pub use quantizer::{ResidualQuantizerConfig, theoretical_bits_per_vector};

/// On-disk format generation for the recovery sidecar.
pub const INDEX_FORMAT_VERSION: u32 = 1;
