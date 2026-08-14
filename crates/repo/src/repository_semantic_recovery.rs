// SPDX-License-Identifier: Apache-2.0
//! Repository adapter for the rebuildable semantic recovery sidecar.

use std::{collections::BTreeSet, path::PathBuf};

use objects::object::{
    DiffKind, LeafPolicy, ObjectSource, State, StateId, diff_trees, resolve_tree_path,
};
use semantic_recovery::{
    Embedder, RecoveryIndex, ResidualQuantizerConfig, StateDocument, StateKey, embed_documents,
};

use crate::{HeddleError, Repository, Result};

const INDEX_RELATIVE_PATH: &str = "indexes/semantic-recovery-v1.bin";
const MAX_CHANGED_PATHS: usize = 48;
const MAX_TEXT_CHARS_PER_BLOB: usize = 16 * 1024;
const MAX_DOCUMENT_CHARS: usize = 256 * 1024;

/// Outcome of a complete semantic recovery sidecar rebuild.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticRecoveryBuildReport {
    /// Indexed state count.
    pub states: usize,
    /// Distinct indexed thread count.
    pub threads: usize,
    /// Ideal residual assignment width (9.0 for the default 32+16 index).
    pub theoretical_bits_per_vector: f64,
    /// Actual bit-packed width (9 for the default 32+16 index).
    pub packed_bits_per_vector: usize,
    /// Complete sidecar size, including codebooks and metadata.
    pub sidecar_bytes: u64,
    /// Local, non-authoritative path written by the rebuild.
    pub path: PathBuf,
}

/// One recovered sibling expressed in repository-native state identity.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticRecoverySibling {
    /// Recovered sibling state.
    pub state: StateId,
    /// Approximate cosine similarity.
    pub similarity: f32,
}

/// Thread inferred from semantically neighboring states.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticThreadReconstruction {
    /// Predicted thread name.
    pub thread: String,
    /// Similarity of the strongest sibling evidence.
    pub confidence: f32,
    /// Nearest states in the predicted thread.
    pub siblings: Vec<SemanticRecoverySibling>,
}

impl Repository {
    /// Path of the rebuildable semantic recovery sidecar.
    pub fn semantic_recovery_index_path(&self) -> PathBuf {
        self.heddle_dir().join(INDEX_RELATIVE_PATH)
    }

    /// Rebuild the complete 32+16 residual-quantized sidecar from current refs.
    pub fn rebuild_semantic_recovery_index<E: Embedder>(
        &self,
        embedder: &mut E,
    ) -> Result<SemanticRecoveryBuildReport> {
        let documents = self.semantic_recovery_documents()?;
        let (index, report) =
            RecoveryIndex::build(&documents, embedder, ResidualQuantizerConfig::default())
                .map_err(recovery_error)?;
        let path = self.semantic_recovery_index_path();
        let sidecar_bytes = index.save(&path).map_err(recovery_error)?;
        Ok(SemanticRecoveryBuildReport {
            states: report.states,
            threads: report.threads,
            theoretical_bits_per_vector: report.theoretical_bits_per_vector,
            packed_bits_per_vector: report.packed_bits_per_vector,
            sidecar_bytes,
            path,
        })
    }

    /// Reconstruct a known state's thread from its nearest indexed siblings.
    ///
    /// A missing sidecar returns `Ok(None)`: recovery metadata never blocks
    /// canonical history reads. Rebuild explicitly to restore the surface.
    pub fn reconstruct_semantic_thread<E: Embedder>(
        &self,
        state: &StateId,
        embedder: &mut E,
        sibling_limit: usize,
    ) -> Result<Option<SemanticThreadReconstruction>> {
        let path = self.semantic_recovery_index_path();
        let index = match RecoveryIndex::load(&path) {
            Ok(index) => index,
            Err(semantic_recovery::RecoveryError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(error) => return Err(recovery_error(error)),
        };
        let document = self.recovery_document_for_state(*state, "query")?;
        let (identity, vectors) = embed_documents(embedder, &[document]).map_err(recovery_error)?;
        if &identity != index.model() {
            return Err(HeddleError::Config(format!(
                "semantic recovery model {} does not match sidecar model {}",
                identity.id,
                index.model().id
            )));
        }
        let result = index
            .reconstruct_thread(state_key(*state), &vectors[0], sibling_limit)
            .map_err(recovery_error)?;
        Ok(result.map(|result| SemanticThreadReconstruction {
            thread: result.thread,
            confidence: result.confidence,
            siblings: result
                .siblings
                .into_iter()
                .map(|neighbor| SemanticRecoverySibling {
                    state: StateId::from_bytes(neighbor.state.0),
                    similarity: neighbor.similarity,
                })
                .collect(),
        }))
    }

    fn semantic_recovery_documents(&self) -> Result<Vec<StateDocument>> {
        let mut thread_names = self.refs().list_threads()?;
        thread_names.sort_by(|left, right| {
            (left.as_str() != "main", left.as_str())
                .cmp(&(right.as_str() != "main", right.as_str()))
        });
        let mut claimed = BTreeSet::new();
        let mut documents = Vec::new();
        for thread in thread_names {
            let Some(mut cursor) = self.refs().get_thread(&thread)? else {
                continue;
            };
            while claimed.insert(cursor) {
                let state = self
                    .store()
                    .get_state(&cursor)?
                    .ok_or(HeddleError::StateNotFound(cursor))?;
                documents.push(self.recovery_document(&state, thread.as_str())?);
                let Some(parent) = state.parents.first() else {
                    break;
                };
                cursor = *parent;
            }
        }
        if documents.is_empty() {
            return Err(HeddleError::Config(
                "semantic recovery needs at least one state reachable from a thread".to_string(),
            ));
        }
        Ok(documents)
    }

    fn recovery_document_for_state(&self, id: StateId, thread: &str) -> Result<StateDocument> {
        let state = self
            .store()
            .get_state(&id)?
            .ok_or(HeddleError::StateNotFound(id))?;
        self.recovery_document(&state, thread)
    }

    fn recovery_document(&self, state: &State, thread: &str) -> Result<StateDocument> {
        let mut text = String::new();
        if let Some(intent) = state.intent.as_deref() {
            text.push_str("intent: ");
            text.push_str(intent);
            text.push('\n');
        } else if let Some(message) = state
            .raw_message
            .as_deref()
            .and_then(|raw| std::str::from_utf8(raw).ok())
        {
            text.push_str("intent: ");
            text.push_str(message.trim());
            text.push('\n');
        }
        if let Some(parent_id) = state.parents.first() {
            let parent = self
                .store()
                .get_state(parent_id)?
                .ok_or(HeddleError::StateNotFound(*parent_id))?;
            let changes = diff_trees(self.store(), &parent.tree, &state.tree).map_err(|error| {
                HeddleError::InvalidObject(format!("tree diff failed: {error}"))
            })?;
            for change in changes.iter().take(MAX_CHANGED_PATHS) {
                text.push_str(&format!("{}: {}\n", change.kind, change.path));
                let tree = if change.kind == DiffKind::Deleted {
                    parent.tree
                } else {
                    state.tree
                };
                append_path_text(self.store(), &tree, &change.path, &mut text)?;
                if text.chars().count() >= MAX_DOCUMENT_CHARS {
                    break;
                }
            }
        }
        if text.trim().is_empty() {
            text = format!("state tree {}", state.tree);
        }
        Ok(StateDocument {
            state: state_key(state.id()),
            thread: thread.to_string(),
            text,
        })
    }
}

fn append_path_text<S: ObjectSource>(
    store: &S,
    tree: &objects::object::ContentHash,
    path: &str,
    output: &mut String,
) -> Result<()> {
    let target = resolve_tree_path(
        store,
        tree,
        std::path::Path::new(path),
        LeafPolicy::LeafContentBlob,
    )?;
    let Some(content) = target.and_then(|target| target.blob) else {
        return Ok(());
    };
    let Some(text) = content.content_str().filter(|text| is_embedding_text(text)) else {
        return Ok(());
    };
    output.extend(text.chars().take(MAX_TEXT_CHARS_PER_BLOB));
    output.push('\n');
    Ok(())
}

fn is_embedding_text(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    let controls = text
        .bytes()
        .filter(|byte| byte.is_ascii_control() && !byte.is_ascii_whitespace())
        .count();
    controls * 50 <= text.len()
}

fn state_key(state: StateId) -> StateKey {
    StateKey(*state.as_bytes())
}

fn recovery_error(error: semantic_recovery::RecoveryError) -> HeddleError {
    HeddleError::Config(format!("semantic recovery sidecar: {error}"))
}

#[cfg(test)]
#[path = "repository_semantic_recovery_tests.rs"]
mod tests;
