// SPDX-License-Identifier: Apache-2.0
//! Immutable source identities and shared position projections.
//!
//! A target's identity is independent of the Thread used to resolve it. Intern
//! a reference against an existing binding before creating a new core: hashing
//! each capture's current coordinates would create a different target.

use serde::{Deserialize, Serialize};
pub mod capture;

use super::{
    AnnotationSourceReference, CollaborationRevision, CollaborationScope,
    CollaborationSourceAnchor, ContentHash,
};

/// Original file evidence. Renames update the file binding, never this core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFileCore {
    pub scope: CollaborationScope,
    pub revision: CollaborationRevision,
    pub path: String,
}

impl SourceFileCore {
    pub fn id(&self) -> Result<ContentHash, SourceTargetError> {
        AnnotationSourceReference {
            scope: self.scope.clone(),
            source: CollaborationSourceAnchor {
                revision: self.revision.clone(),
                path: self.path.clone(),
                symbol_id: String::new(),
                start_line: None,
                end_line: None,
                target: None,
            },
        }
        .validate()
        .map_err(|error| SourceTargetError::Invalid(error.to_string()))?;
        identity("heddle-source-file-v1", self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTargetCore {
    pub file: ContentHash,
    /// Exact original revision for the selector's symbol or positions.
    pub revision: CollaborationRevision,
    pub selector: SourceSelector,
}

impl SourceTargetCore {
    pub fn id(&self) -> Result<ContentHash, SourceTargetError> {
        // Use the same exact-revision validator as signed collaboration.
        CollaborationSourceAnchor {
            revision: self.revision.clone(),
            path: String::new(),
            symbol_id: String::new(),
            start_line: None,
            end_line: None,
            target: None,
        }
        .validate()
        .map_err(|error| SourceTargetError::Invalid(error.to_string()))?;
        match &self.selector {
            SourceSelector::File => {}
            SourceSelector::Symbol { address } => {
                if address.trim().is_empty()
                    || address.len() > 4096
                    || address.chars().any(char::is_control)
                {
                    return Err(invalid("invalid source symbol address"));
                }
            }
            SourceSelector::Lines { range } => range.validate()?,
        }
        identity("heddle-source-target-v1", self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum SourceSelector {
    File,
    Symbol { address: String },
    Lines { range: SourceLineRange },
}

/// Shared by primary anchors and reference tags. Scope selection is explicit;
/// the authoring scope remains in the immutable core and signed operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTargetReference {
    pub target: ContentHash,
    pub binding: SourceTargetBinding,
}

impl SourceTargetReference {
    /// Intern exact original evidence once. A materialized anchor preserves its
    /// existing identity even when a caller explicitly selects another binding.
    pub fn from_source(
        source: &AnnotationSourceReference,
        binding: SourceTargetBinding,
    ) -> Result<Self, SourceTargetError> {
        source
            .validate()
            .map_err(|error| invalid(&error.to_string()))?;
        if !source.source.symbol_id.is_empty() && source.source.symbol_id.trim().is_empty() {
            return Err(invalid("invalid source symbol address"));
        }
        let target = if let Some(existing) = &source.source.target {
            existing.target
        } else {
            let file = SourceFileCore {
                scope: source.scope.clone(),
                revision: source.source.revision.clone(),
                path: source.source.path.clone(),
            };
            let selector = if !source.source.symbol_id.is_empty() {
                SourceSelector::Symbol {
                    address: source.source.symbol_id.clone(),
                }
            } else if let (Some(start), Some(end)) =
                (source.source.start_line, source.source.end_line)
            {
                SourceSelector::Lines {
                    range: SourceLineRange {
                        start: start
                            .checked_sub(1)
                            .ok_or_else(|| invalid("source lines are one-based"))?,
                        end,
                        start_affinity: SourceAffinity::After,
                        end_affinity: SourceAffinity::Before,
                    },
                }
            } else {
                SourceSelector::File
            };
            SourceTargetCore {
                file: file.id()?,
                revision: file.revision,
                selector,
            }
            .id()?
        };
        let reference = Self { target, binding };
        reference.validate()?;
        Ok(reference)
    }
    pub fn validate(&self) -> Result<(), SourceTargetError> {
        match &self.binding {
            SourceTargetBinding::ViewedThread => Ok(()),
            SourceTargetBinding::NamedThread { scope } => {
                self.binding.scope(scope)?;
                Ok(())
            }
            SourceTargetBinding::PinnedRevision { scope, revision } => {
                self.binding.scope(scope)?;
                CollaborationSourceAnchor {
                    revision: revision.clone(),
                    path: String::new(),
                    symbol_id: String::new(),
                    start_line: None,
                    end_line: None,
                    target: None,
                }
                .validate()
                .map_err(|error| invalid(&error.to_string()))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum SourceTargetBinding {
    ViewedThread,
    NamedThread {
        scope: CollaborationScope,
    },
    PinnedRevision {
        scope: CollaborationScope,
        revision: CollaborationRevision,
    },
}

impl SourceTargetBinding {
    /// Selects identity only. The caller must authorize source access, verify
    /// inheritance, and choose a revision/frontier in the returned Thread.
    pub fn scope<'a>(
        &'a self,
        viewed_thread: &'a CollaborationScope,
    ) -> Result<&'a CollaborationScope, SourceTargetError> {
        let scope = match self {
            Self::ViewedThread => viewed_thread,
            Self::NamedThread { scope } | Self::PinnedRevision { scope, .. } => scope,
        };
        if scope.spool.is_nil()
            || (scope.thread.is_none() && !matches!(self, Self::PinnedRevision { .. }))
        {
            return Err(invalid("tracking requires a concrete Thread and spool"));
        }
        Ok(scope)
    }
}

/// Which side of an insertion at this exact boundary the endpoint follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAffinity {
    Before,
    After,
}

/// Zero-based, half-open line boundaries. Inclusive display lines a..b become
/// [a - 1, b). A default non-growing range uses After at start, Before at end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLineRange {
    pub start: u32,
    pub end: u32,
    pub start_affinity: SourceAffinity,
    pub end_affinity: SourceAffinity,
}

impl SourceLineRange {
    pub fn validate(&self) -> Result<(), SourceTargetError> {
        if self.start >= self.end {
            return Err(invalid("source range must contain at least one line"));
        }
        Ok(())
    }
}

/// A single replacement in original/new line coordinates. Adjacent operations
/// must be coalesced so each boundary has exactly one mapping decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLineEdit {
    pub old_start: u32,
    pub old_end: u32,
    pub new_start: u32,
    pub new_end: u32,
}

/// Stored once per changed blob pair, never once per referring range. Validated
/// on construction/decode, so each projection uses binary search over edits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "LineEditMapWire")]
pub struct SourceLineEditMap {
    old_lines: u32,
    new_lines: u32,
    edits: Vec<SourceLineEdit>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LineEditMapWire {
    old_lines: u32,
    new_lines: u32,
    edits: Vec<SourceLineEdit>,
}

impl TryFrom<LineEditMapWire> for SourceLineEditMap {
    type Error = SourceTargetError;
    fn try_from(wire: LineEditMapWire) -> Result<Self, Self::Error> {
        Self::new(wire.old_lines, wire.new_lines, wire.edits)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceRangeProjection {
    Resolved {
        range: SourceLineRange,
        changed: bool,
    },
    Deleted,
    /// An endpoint fell inside replaced material; there is no proven boundary.
    Ambiguous,
}

impl SourceLineEditMap {
    pub fn new(
        old_lines: u32,
        new_lines: u32,
        edits: Vec<SourceLineEdit>,
    ) -> Result<Self, SourceTargetError> {
        if edits.len() > 65_536 {
            return Err(invalid("line edit map exceeds its operation budget"));
        }
        let mut old_end = 0;
        let mut new_end = 0;
        for (index, edit) in edits.iter().enumerate() {
            if edit.old_start > edit.old_end
                || edit.new_start > edit.new_end
                || edit.old_end > old_lines
                || edit.new_end > new_lines
                || edit.old_start < old_end
                || edit.new_start < new_end
                || edit.old_start - old_end != edit.new_start - new_end
                || (index > 0 && edit.old_start == old_end)
                || (edit.old_start == edit.old_end && edit.new_start == edit.new_end)
            {
                return Err(invalid("invalid, overlapping or uncoalesced line edits"));
            }
            old_end = edit.old_end;
            new_end = edit.new_end;
        }
        if old_lines - old_end != new_lines - new_end {
            return Err(invalid(
                "line edit map does not cover the source transition",
            ));
        }
        Ok(Self {
            old_lines,
            new_lines,
            edits,
        })
    }

    pub fn edits(&self) -> &[SourceLineEdit] {
        &self.edits
    }

    /// O(log edits); this does not parse text, scan edits, or mutate references.
    pub fn project(
        &self,
        range: SourceLineRange,
    ) -> Result<SourceRangeProjection, SourceTargetError> {
        range.validate()?;
        if range.end > self.old_lines {
            return Err(invalid("source range exceeds original file"));
        }
        let first = self
            .edits
            .partition_point(|edit| edit.old_end < range.start);
        if self.edits.get(first).is_some_and(|edit| {
            edit.old_start <= range.start
                && edit.old_end >= range.end
                && edit.new_start == edit.new_end
        }) {
            return Ok(SourceRangeProjection::Deleted);
        }
        let (Some(start), Some(end)) = (
            self.boundary(range.start, range.start_affinity),
            self.boundary(range.end, range.end_affinity),
        ) else {
            return Ok(SourceRangeProjection::Ambiguous);
        };
        if start >= end {
            return Ok(SourceRangeProjection::Deleted);
        }
        // Find the first edit which affects selected content. Merely shifting
        // both endpoints from an earlier insertion does not change its body.
        let affecting = self.edits.partition_point(|edit| {
            edit.old_end < range.start
                || (edit.old_end == range.start
                    && (edit.old_start < edit.old_end
                        || range.start_affinity == SourceAffinity::After))
        });
        let changed = self.edits.get(affecting).is_some_and(|edit| {
            edit.old_start < range.end
                || (edit.old_start == range.end
                    && edit.old_start == edit.old_end
                    && range.end_affinity == SourceAffinity::After)
        });
        Ok(SourceRangeProjection::Resolved {
            range: SourceLineRange {
                start,
                end,
                ..range
            },
            changed,
        })
    }

    fn boundary(&self, position: u32, affinity: SourceAffinity) -> Option<u32> {
        let index = self.edits.partition_point(|edit| edit.old_end < position);
        if let Some(edit) = self.edits.get(index)
            && edit.old_start <= position
        {
            return if edit.old_start == edit.old_end {
                Some(match affinity {
                    SourceAffinity::Before => edit.new_start,
                    SourceAffinity::After => edit.new_end,
                })
            } else if position == edit.old_start {
                Some(edit.new_start)
            } else if position == edit.old_end {
                Some(edit.new_end)
            } else {
                None
            };
        }
        match index.checked_sub(1).and_then(|prior| self.edits.get(prior)) {
            Some(prior) => prior.new_end.checked_add(position - prior.old_end),
            None => Some(position),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SourceTargetError {
    #[error("{0}")]
    Invalid(String),
    #[error("source target encoding: {0}")]
    Encoding(#[from] rmp_serde::encode::Error),
}

fn identity(domain: &str, value: &impl Serialize) -> Result<ContentHash, SourceTargetError> {
    Ok(ContentHash::compute_typed(
        domain,
        &rmp_serde::to_vec_named(value)?,
    ))
}

fn invalid(message: &str) -> SourceTargetError {
    SourceTargetError::Invalid(message.into())
}

#[cfg(test)]
#[path = "source_target_tests.rs"]
mod tests;
