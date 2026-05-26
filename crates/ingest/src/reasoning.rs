// SPDX-License-Identifier: Apache-2.0
//! [`ReasoningPoint`] — the unit of extracted agent reasoning.
//!
//! Produced by the TS two-stage extractor (`tools/extract-reasoning/`), consumed
//! here to be written as a Heddle annotation. Keeping the schema in Rust gives
//! the extractor a source-of-truth to marshal into via `serde_json`.
//!
//! The shape is deliberately small. A `ReasoningPoint` should fit on a
//! notecard — if it doesn't, it belongs in a design doc, not an annotation.
//!
//! ## Kinds
//!
//! Points carry the same [`AnnotationKind`] the product surfaces. See the
//! enum's docs for when each applies; the extractor picks one via keyword
//! heuristics in `reasoning_extract.rs`.

use objects::object::AnnotationKind;
use serde::{Deserialize, Serialize};

/// Where a [`ReasoningPoint`] attaches in the code. Prefer `symbol` for
/// resilience to line drift; fall back to `line_range` otherwise. A point
/// with neither is file-scoped.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReasoningTarget {
    /// Repo-relative path, using `/` as separator.
    pub file: String,
    /// Enclosing function / type / const name, when resolvable via tree-sitter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Inclusive line range on the post-change blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_range: Option<(u32, u32)>,
}

/// Provenance for a [`ReasoningPoint`] — never shown to future agents by
/// default, but auditors can traverse to the originating transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReasoningEvidence {
    /// Transcript session this was extracted from.
    pub session_id: String,
    /// `(first_turn_index, last_turn_index)` within the session.
    pub turn_range: (u32, u32),
    /// The git commit SHA that materialized this point.
    pub commit_sha: String,
    /// Provider that emitted the transcript (`"claude"` / `"codex"`).
    pub provider: String,
}

/// A single extracted reasoning point, ready to be written as a Heddle
/// annotation or stored as JSON for the extractor → importer handoff.
///
/// `Eq` isn't derived because `confidence: f32` — use `==` via `PartialEq`
/// (tests compare concrete instances round-tripped from the same f32 bits,
/// so equality is still reliable in practice).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReasoningPoint {
    pub kind: AnnotationKind,
    /// ≤ 140 chars, imperative voice. Enforced at extractor-stage-2.
    pub text: String,
    pub target: ReasoningTarget,
    pub evidence: ReasoningEvidence,
    /// 0.0–1.0. Stage-2 of the extractor rewrites low-confidence points or
    /// drops them entirely; anything surviving here is ≥ `keep_threshold`.
    pub confidence: f32,
}

impl ReasoningPoint {
    /// Returns `true` if the `text` fits within the 140-char budget.
    pub fn is_well_formed(&self) -> bool {
        !self.text.is_empty()
            && self.text.chars().count() <= 140
            && (0.0..=1.0).contains(&self.confidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ReasoningPoint {
        ReasoningPoint {
            kind: AnnotationKind::Invariant,
            text: "never call parseToken without a tenant scope".into(),
            target: ReasoningTarget {
                file: "crates/server/src/auth/token.rs".into(),
                symbol: Some("parseToken".into()),
                line_range: Some((42, 88)),
            },
            evidence: ReasoningEvidence {
                session_id: "019d2171-c8a0-7ae2-9dc2-fd9e86d4f701".into(),
                turn_range: (7, 12),
                commit_sha: "ca1af22000000000000000000000000000000000".into(),
                provider: "claude".into(),
            },
            confidence: 0.86,
        }
    }

    #[test]
    fn well_formed_round_trip() {
        let p = sample();
        assert!(p.is_well_formed());
        let json = serde_json::to_string(&p).unwrap();
        let back: ReasoningPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn rejects_over_budget_text() {
        let mut p = sample();
        p.text = "x".repeat(141);
        assert!(!p.is_well_formed());
    }

    #[test]
    fn rejects_empty_text() {
        let mut p = sample();
        p.text = String::new();
        assert!(!p.is_well_formed());
    }

    #[test]
    fn rejects_out_of_range_confidence() {
        let mut p = sample();
        p.confidence = 1.5;
        assert!(!p.is_well_formed());
    }

    #[test]
    fn kind_serializes_lowercase() {
        let p = sample();
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["kind"], "invariant");
    }

    #[test]
    fn optional_target_fields_skip_when_none() {
        let mut p = sample();
        p.target.symbol = None;
        p.target.line_range = None;
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("symbol"));
        assert!(!json.contains("line_range"));
    }
}
