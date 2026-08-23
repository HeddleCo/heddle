// SPDX-License-Identifier: Apache-2.0
//! Invariant-adjacency: fires when the changed state has annotations
//! whose kind is `Invariant`, or whose tags include `"enforces"`.
//!
//! Pure module: it inspects the new state's annotation set (already
//! decoded into the new state's `context` blob by the caller) to decide
//! whether to fire. Annotations are passed through the `SemanticContext`
//! since this module doesn't need source parsing.

use objects::object::{
    AnnotationKind, ProducerId, RiskSignal, RiskSignalKind, SignalAnchor, State,
};

use crate::{config::ReviewSignalsConfig, registry::SemanticContext};

const VERSION: u32 = 1;
const MODULE_ID: &str = "invariant_adjacency";
const REASON_PREFIX: &str = "invariant annotation lives on a changed symbol";

pub fn run(
    _prior: &State,
    new: &State,
    cfg: &ReviewSignalsConfig,
    ctx: &SemanticContext,
) -> Vec<RiskSignal> {
    if !cfg.invariant_adjacency.enabled {
        return Vec::new();
    }
    let annotations = ctx_annotations(ctx);
    let computed_at = new
        .authored_at
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|| new.created_at.timestamp());
    let mut out = Vec::new();
    for annotation in annotations {
        let fires = matches!(annotation.kind, AnnotationKind::Invariant)
            || annotation.tags.iter().any(|t| t == "enforces");
        if !fires {
            continue;
        }
        let anchor = annotation.anchor.clone();
        let excerpt: String = annotation.content.chars().take(120).collect();
        let reason = format!("{REASON_PREFIX}: {excerpt}");
        out.push(RiskSignal {
            kind: RiskSignalKind::InvariantAdjacency,
            anchor,
            reason: truncate_reason(&reason),
            producer: ProducerId::new(MODULE_ID, VERSION),
            computed_at,
            computed_against: Some(new.state_id),
        });
    }
    out
}

fn ctx_annotations(ctx: &SemanticContext) -> &[InvariantAnnotation] {
    &ctx.invariant_annotations
}

/// Compact representation the module operates on. Lifted from the W1
/// annotation type so the module stays decoupled from the storage shape.
#[derive(Debug, Clone)]
pub struct InvariantAnnotation {
    pub anchor: SignalAnchor,
    pub kind: AnnotationKind,
    pub content: String,
    pub tags: Vec<String>,
}

use crate::truncate_reason;

#[cfg(test)]
mod tests {
    use objects::object::{Attribution, ContentHash, Principal};

    use crate::{registry::SemanticContext, ReviewSignalsConfig};

    use super::*;

    fn empty_state() -> State {
        State::new_snapshot(
            ContentHash::compute(b"tree"),
            vec![],
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        )
    }

    fn enabled_cfg() -> ReviewSignalsConfig {
        ReviewSignalsConfig::default()
    }

    fn ctx_with(annotations: Vec<InvariantAnnotation>) -> SemanticContext {
        SemanticContext {
            invariant_annotations: annotations,
            ..SemanticContext::default()
        }
    }

    #[test]
    fn run_fires_when_invariant_annotation_present() {
        let prior = empty_state();
        let new = empty_state();
        let ctx = ctx_with(vec![InvariantAnnotation {
            anchor: SignalAnchor::symbol("src/lib.rs", "foo"),
            kind: AnnotationKind::Invariant,
            content: "must hold across operations".to_string(),
            tags: vec![],
        }]);
        let signals = run(&prior, &new, &enabled_cfg(), &ctx);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, RiskSignalKind::InvariantAdjacency);
        assert!(signals[0].reason.contains("must hold across operations"));
    }

    #[test]
    fn run_fires_when_enforces_tag_present() {
        let prior = empty_state();
        let new = empty_state();
        let ctx = ctx_with(vec![InvariantAnnotation {
            anchor: SignalAnchor::symbol("src/lib.rs", "foo"),
            kind: AnnotationKind::Constraint,
            content: "tagged as enforced".to_string(),
            tags: vec!["enforces".to_string()],
        }]);
        let signals = run(&prior, &new, &enabled_cfg(), &ctx);
        assert_eq!(signals.len(), 1);
    }

    #[test]
    fn run_quiet_when_no_invariant_or_enforces() {
        let prior = empty_state();
        let new = empty_state();
        let ctx = ctx_with(vec![InvariantAnnotation {
            anchor: SignalAnchor::symbol("src/lib.rs", "foo"),
            kind: AnnotationKind::Rationale,
            content: "design decision".to_string(),
            tags: vec!["history".to_string()],
        }]);
        let signals = run(&prior, &new, &enabled_cfg(), &ctx);
        assert!(signals.is_empty());
    }

    #[test]
    fn run_quiet_when_context_has_no_annotations() {
        // The prior no-op: empty SemanticContext must produce no signals,
        // and the module must stay quiet when the context carries nothing.
        let prior = empty_state();
        let new = empty_state();
        let ctx = SemanticContext::default();
        let signals = run(&prior, &new, &enabled_cfg(), &ctx);
        assert!(signals.is_empty());
    }

    #[test]
    fn fires_when_invariant_annotation_present() {
        let new = empty_state();
        let annotations = vec![InvariantAnnotation {
            anchor: SignalAnchor::symbol("src/lib.rs", "foo"),
            kind: AnnotationKind::Invariant,
            content: "must hold across operations".to_string(),
            tags: vec![],
        }];
        let signals = synthetic_run(&new, &annotations);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, RiskSignalKind::InvariantAdjacency);
        assert!(signals[0].reason.contains("must hold across operations"));
    }

    #[test]
    fn fires_when_enforces_tag_present() {
        let new = empty_state();
        let annotations = vec![InvariantAnnotation {
            anchor: SignalAnchor::symbol("src/lib.rs", "foo"),
            kind: AnnotationKind::Constraint,
            content: "tagged as enforced".to_string(),
            tags: vec!["enforces".to_string()],
        }];
        let signals = synthetic_run(&new, &annotations);
        assert_eq!(signals.len(), 1);
    }

    #[test]
    fn quiet_when_no_invariant_or_enforces() {
        let new = empty_state();
        let annotations = vec![InvariantAnnotation {
            anchor: SignalAnchor::symbol("src/lib.rs", "foo"),
            kind: AnnotationKind::Rationale,
            content: "design decision".to_string(),
            tags: vec!["history".to_string()],
        }];
        let signals = synthetic_run(&new, &annotations);
        assert!(signals.is_empty());
    }

    fn synthetic_run(new: &State, annotations: &[InvariantAnnotation]) -> Vec<RiskSignal> {
        let computed_at = new
            .authored_at
            .map(|dt| dt.timestamp())
            .unwrap_or_else(|| new.created_at.timestamp());
        annotations
            .iter()
            .filter(|a| {
                matches!(a.kind, AnnotationKind::Invariant)
                    || a.tags.iter().any(|t| t == "enforces")
            })
            .map(|a| {
                let excerpt: String = a.content.chars().take(120).collect();
                let reason = format!("{REASON_PREFIX}: {excerpt}");
                RiskSignal {
                    kind: RiskSignalKind::InvariantAdjacency,
                    anchor: a.anchor.clone(),
                    reason: truncate_reason(&reason),
                    producer: ProducerId::new(MODULE_ID, VERSION),
                    computed_at,
                    computed_against: Some(new.state_id),
                }
            })
            .collect()
    }
}
