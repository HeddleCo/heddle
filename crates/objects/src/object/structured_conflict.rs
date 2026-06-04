// SPDX-License-Identifier: Apache-2.0
//! Merge conflicts as structured data.
//!
//! Today, merge conflicts surface only as text markers in the working tree
//! (`<<<<<<<` / `=======` / `>>>>>>>`). That works for humans with editors,
//! and is unworkable for agents that need to resolve conflicts
//! programmatically without parsing markers.
//!
//! [`StructuredConflict`] makes the conflict itself first-class: the
//! conflicting symbol, the three sides (base / ours / theirs), and any
//! candidate resolutions an upstream module suggested. The text-marker
//! representation in the working tree is *one rendering* of this object, not
//! its source of truth — see `crates/repo/src/merge_state.rs::render_text_markers`.

use serde::{Deserialize, Serialize};

use crate::object::{
    hash::{ChangeId, ContentHash},
    state_review::SymbolAnchor,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredConflict {
    pub format_version: u8,
    pub conflicts: Vec<ConflictSymbol>,
}

versioned_msgpack_blob! {
    blob: StructuredConflict,
    item: ConflictSymbol,
    field: conflicts,
    error: ConflictError,
    codec_err: Encoding,
    version: 1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictSymbol {
    /// Stable id for this specific conflict (e.g., a UUID); used by
    /// `heddle conflict resolve <id>` to address it without parsing
    /// file paths or line numbers.
    pub id: String,
    pub anchor: SymbolAnchor,
    pub base: ConflictSide,
    pub ours: ConflictSide,
    pub theirs: ConflictSide,
    /// Auto-detected candidate resolutions, in display order. The list may
    /// be empty when no candidate is obvious.
    #[serde(default)]
    pub candidate_resolutions: Vec<ConflictResolution>,
}

impl ConflictSymbol {
    pub fn validate(&self) -> Result<(), ConflictError> {
        if self.id.is_empty() {
            return Err(ConflictError::EmptyId);
        }
        if self.anchor.file.is_empty() {
            return Err(ConflictError::EmptyAnchorFile);
        }
        if self.anchor.symbol.is_empty() {
            return Err(ConflictError::EmptyAnchorSymbol);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictSide {
    /// The state this side originated from. `None` is permitted only for the
    /// base when there is no common ancestor.
    #[serde(default)]
    pub source_state: Option<ChangeId>,
    pub body: String,
    pub body_hash: ContentHash,
}

impl ConflictSide {
    pub fn from_body(source_state: Option<ChangeId>, body: impl Into<String>) -> Self {
        let body = body.into();
        let body_hash = ContentHash::compute(body.as_bytes());
        Self {
            source_state,
            body,
            body_hash,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictResolution {
    TakeOurs,
    TakeTheirs,
    TakeBase,
    Custom { body: String, rationale: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ConflictError {
    #[error("unsupported structured conflict version {0}")]
    UnsupportedVersion(u8),
    #[error("conflict id must not be empty")]
    EmptyId,
    #[error("conflict anchor must reference a non-empty file")]
    EmptyAnchorFile,
    #[error("conflict anchor must reference a non-empty symbol")]
    EmptyAnchorSymbol,
    #[error("structured conflict encoding error: {0}")]
    Encoding(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_conflict() -> ConflictSymbol {
        ConflictSymbol {
            id: "c-1".into(),
            anchor: SymbolAnchor::new("src/lib.rs", "merge_target"),
            base: ConflictSide::from_body(Some(ChangeId::from_bytes([1; 16])), "fn x() { 0 }"),
            ours: ConflictSide::from_body(Some(ChangeId::from_bytes([2; 16])), "fn x() { 1 }"),
            theirs: ConflictSide::from_body(Some(ChangeId::from_bytes([3; 16])), "fn x() { 2 }"),
            candidate_resolutions: vec![
                ConflictResolution::TakeOurs,
                ConflictResolution::TakeTheirs,
            ],
        }
    }

    #[test]
    fn three_way_conflict_roundtrip() {
        let blob = StructuredConflict::new(vec![sample_conflict()]);
        let bytes = blob.encode().unwrap();
        let decoded = StructuredConflict::decode(&bytes).unwrap();
        assert_eq!(blob, decoded);
    }

    #[test]
    fn empty_conflicts_list_validates() {
        let blob = StructuredConflict::new(vec![]);
        blob.validate().unwrap();
    }

    #[test]
    fn empty_id_rejected() {
        let mut c = sample_conflict();
        c.id = String::new();
        assert!(matches!(c.validate(), Err(ConflictError::EmptyId)));
    }

    #[test]
    fn body_hash_matches_body() {
        let side = ConflictSide::from_body(None, "fn x() { 0 }");
        assert_eq!(side.body_hash, ContentHash::compute(b"fn x() { 0 }"));
    }

    #[test]
    fn future_version_rejected() {
        let blob = StructuredConflict {
            format_version: StructuredConflict::FORMAT_VERSION + 1,
            conflicts: vec![],
        };
        assert!(matches!(
            blob.validate(),
            Err(ConflictError::UnsupportedVersion(_))
        ));
    }
}
