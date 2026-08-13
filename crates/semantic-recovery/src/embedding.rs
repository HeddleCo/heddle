// SPDX-License-Identifier: Apache-2.0
//! Offline BGE-small model loading and the embedding abstraction used by tests.

use std::{fs, path::Path};

use fastembed::{
    InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{RecoveryError, Result};

/// Dimension of `BAAI/bge-small-en-v1.5` embeddings.
pub const BGE_SMALL_DIMENSIONS: usize = 384;
/// Exact optimized ONNX artifact selected by the semantic-layer investigation.
pub const BGE_SMALL_ARTIFACT_SHA256: &str =
    "51f1bd0addd6e859e42c2c8021a5e5461385bb676a649f4b269aa445449f2431";
/// Stable model identity recorded in every sidecar.
pub const BGE_SMALL_MODEL_ID: &str = concat!(
    "qdrant/bge-small-en-v1.5-onnx-q@",
    "52398278842ec682c6f32300af41344b1c0b0bb2/model_optimized.onnx"
);

/// Model identity persisted with an index so incompatible sidecars fail loud.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIdentity {
    /// Repository/revision/artifact identity.
    pub id: String,
    /// SHA-256 of the ONNX artifact.
    pub artifact_sha256: String,
    /// Output vector dimension.
    pub dimensions: usize,
}

/// Local text embedding boundary.
pub trait Embedder {
    /// Identity of the model and artifact used for inference.
    fn identity(&self) -> ModelIdentity;
    /// Embed a batch of texts locally.
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Pinned, local-only BGE-small ONNX embedder.
pub struct BgeSmallEmbedder {
    model: TextEmbedding,
}

impl BgeSmallEmbedder {
    /// Load the pinned model from local files without an HTTP-capable model hub.
    pub fn from_model_dir(model_dir: impl AsRef<Path>) -> Result<Self> {
        let model_dir = model_dir.as_ref();
        let onnx = read_asset(model_dir, "model_optimized.onnx")?;
        let actual = Sha256::digest(&onnx)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if actual != BGE_SMALL_ARTIFACT_SHA256 {
            return Err(RecoveryError::ModelDigestMismatch {
                expected: BGE_SMALL_ARTIFACT_SHA256,
                actual,
            });
        }

        let tokenizer_files = TokenizerFiles {
            tokenizer_file: read_asset(model_dir, "tokenizer.json")?,
            config_file: read_asset(model_dir, "config.json")?,
            special_tokens_map_file: read_asset(model_dir, "special_tokens_map.json")?,
            tokenizer_config_file: read_asset(model_dir, "tokenizer_config.json")?,
        };
        let definition = UserDefinedEmbeddingModel::new(onnx, tokenizer_files)
            .with_pooling(Pooling::Cls)
            .with_quantization(QuantizationMode::Static);
        let model = TextEmbedding::try_new_from_user_defined(
            definition,
            InitOptionsUserDefined::new().with_max_length(512),
        )
        .map_err(|error| RecoveryError::Embedding(error.to_string()))?;
        Ok(Self { model })
    }
}

impl Embedder for BgeSmallEmbedder {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            id: BGE_SMALL_MODEL_ID.to_string(),
            artifact_sha256: BGE_SMALL_ARTIFACT_SHA256.to_string(),
            dimensions: BGE_SMALL_DIMENSIONS,
        }
    }

    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.model
            .embed(texts, Some(256))
            .map_err(|error| RecoveryError::Embedding(error.to_string()))
    }
}

fn read_asset(model_dir: &Path, name: &str) -> Result<Vec<u8>> {
    let path = model_dir.join(name);
    fs::read(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RecoveryError::MissingModelAsset(path)
        } else {
            RecoveryError::Io(error)
        }
    })
}
