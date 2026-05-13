// SPDX-License-Identifier: Apache-2.0
//! Line-level provenance for text files.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Attribution, ChangeId, ContentHash};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProvenance {
    pub format_version: u8,
    pub file_blob: ContentHash,
    pub line_count: u32,
    pub origins: Vec<Origin>,
    pub origin_sets: Vec<OriginSet>,
    pub spans: Vec<LineSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub state_id: ChangeId,
    pub attribution: Attribution,
    /// Committer time — when the state object came into being. Stable
    /// across re-imports because it's part of the state hash.
    pub created_at: DateTime<Utc>,
    /// Authoring time, when distinct from `created_at`. Populated by
    /// the git-ingest importer from the commit's `authored_at` so
    /// blame can match git's default of showing author time. Native
    /// heddle commits leave this `None` and blame falls back to
    /// `created_at`. Tail-only optional field for forward compat.
    #[serde(default)]
    pub authored_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginSet {
    pub origin_indexes: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineSpan {
    pub start_line: u32,
    pub line_len: u32,
    pub origin_set_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProvenanceError {
    #[error("unsupported provenance format version {0}")]
    UnsupportedVersion(u8),
    #[error("line spans do not cover the file exactly")]
    InvalidCoverage,
    #[error("invalid origin set index {0}")]
    InvalidOriginSetIndex(u32),
    #[error("invalid origin index {0}")]
    InvalidOriginIndex(u32),
    #[error("provenance file blob mismatch")]
    BlobMismatch,
    #[error("provenance line count mismatch")]
    LineCountMismatch,
}

impl FileProvenance {
    pub const FORMAT_VERSION: u8 = 1;

    pub fn new(
        file_blob: ContentHash,
        line_count: u32,
        spans: Vec<LineSpan>,
        origins: Vec<Origin>,
        origin_sets: Vec<OriginSet>,
    ) -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            file_blob,
            line_count,
            origins,
            origin_sets,
            spans,
        }
    }

    pub fn validate(&self) -> Result<(), ProvenanceError> {
        if self.format_version != Self::FORMAT_VERSION {
            return Err(ProvenanceError::UnsupportedVersion(self.format_version));
        }

        let mut next_line = 0u32;
        for span in &self.spans {
            if span.start_line != next_line || span.line_len == 0 {
                return Err(ProvenanceError::InvalidCoverage);
            }
            let Some(origin_set) = self.origin_sets.get(span.origin_set_index as usize) else {
                return Err(ProvenanceError::InvalidOriginSetIndex(
                    span.origin_set_index,
                ));
            };
            for origin_index in &origin_set.origin_indexes {
                if self.origins.get(*origin_index as usize).is_none() {
                    return Err(ProvenanceError::InvalidOriginIndex(*origin_index));
                }
            }
            next_line = next_line.saturating_add(span.line_len);
        }

        if next_line != self.line_count {
            return Err(ProvenanceError::InvalidCoverage);
        }

        Ok(())
    }

    pub fn line_origin_set_indexes(&self) -> Result<Vec<u32>, ProvenanceError> {
        self.validate()?;
        let mut out = Vec::with_capacity(self.line_count as usize);
        for span in &self.spans {
            for _ in 0..span.line_len {
                out.push(span.origin_set_index);
            }
        }
        Ok(out)
    }
}