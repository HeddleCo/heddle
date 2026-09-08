use std::collections::BTreeSet;

use super::*;
use crate::{SemanticParseCache, parser::Language};

pub(super) fn run(
    path: &str,
    base: Option<&str>,
    source: Option<&str>,
    symbols: &[String],
    out: &mut Comparison,
) {
    let problem = if !path.ends_with(".rs") {
        Some(Limitation::Language)
    } else if base.is_none() || source.is_none() {
        Some(Limitation::MissingSource)
    } else if [base, source]
        .iter()
        .flatten()
        .any(|s| s.len() > MAX_SOURCE_BYTES)
    {
        Some(Limitation::Budget)
    } else {
        None
    };
    if let Some(problem) = problem {
        omit(out, problem);
        return;
    }
    let cache = SemanticParseCache::shared();
    let (Some(base), Some(source)) = (
        base.and_then(|s| cache.parse(s, Language::Rust)),
        source.and_then(|s| cache.parse(s, Language::Rust)),
    ) else {
        omit(out, Limitation::ParseError);
        return;
    };
    let (mut old, old_exhausted) = extract::extract(path, &base, Side::Base, symbols);
    let (mut new, new_exhausted) = extract::extract(path, &source, Side::Source, symbols);
    // Never pair a target from a truncated inventory: a duplicate may have
    // been omitted, so even the retained prefix is not safe correspondence.
    if !old_exhausted || !new_exhausted {
        omit(out, Limitation::Budget);
        return;
    }
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).cloned().collect();
    if keys.is_empty() {
        out.analyzed.push(Scope {
            symbol_address: String::new(),
            target: String::new(),
            support: Support::Supported,
            limitations: vec![],
            sources: vec![],
        });
    }
    for (symbol, target) in keys {
        let old = old
            .remove(&(symbol.clone(), target.clone()))
            .unwrap_or_default();
        let new = new
            .remove(&(symbol.clone(), target.clone()))
            .unwrap_or_default();
        let mut scope = Scope {
            symbol_address: symbol.clone(),
            target,
            support: Support::Supported,
            limitations: vec![],
            sources: old
                .iter()
                .chain(&new)
                .flat_map(|c| c.assignments.iter().map(|a| a.source.clone()))
                .collect(),
        };
        // Full group equality suppresses comment/whitespace-only edits even
        // where duplicate targets prevent confident cross-revision matching.
        let identical = old.len() == new.len()
            && (base.source() == source.source()
                || old.iter().zip(&new).all(|(a, b)| equivalent(a, b)));
        if identical {
            out.analyzed.push(scope);
            continue;
        }
        let conditional = old
            .iter()
            .chain(&new)
            .any(|c| c.expressions.iter().any(|e| e.conditional.is_some()));
        if !conditional {
            scope.support = Support::Unsupported;
            scope.limitations.push(Limitation::Syntax);
            out.omitted.push(scope);
            continue;
        }
        let ambiguous = old.len() > 1 || new.len() > 1;
        let mut change = Change {
            id: String::new(),
            symbol_address: symbol,
            support: Support::Supported,
            limitations: vec![],
            operations: vec![],
            assignments: vec![],
            expressions: vec![],
            bindings: vec![],
            correspondences: vec![],
        };
        let old_ids = old
            .iter()
            .flat_map(|c| c.assignments.iter().map(|a| a.value_id.clone()))
            .collect::<Vec<_>>();
        let new_ids = new
            .iter()
            .flat_map(|c| c.assignments.iter().map(|a| a.value_id.clone()))
            .collect::<Vec<_>>();
        for c in old.into_iter().chain(new) {
            change.operations.extend(c.operations);
            change.assignments.extend(c.assignments);
            change.expressions.extend(c.expressions);
            change.bindings.extend(c.bindings);
            for limitation in c.limitations {
                if !change.limitations.contains(&limitation) {
                    change.limitations.push(limitation);
                }
            }
        }
        change.id = extract::hash(&[
            path.as_bytes(),
            base.content_hash().as_bytes(),
            source.content_hash().as_bytes(),
            change.symbol_address.as_bytes(),
            scope.target.as_bytes(),
            &COMPARISON_VERSION.to_le_bytes(),
            binding_key(extraction_key(ContentHash::compute_typed(
                "blob",
                base.source().as_bytes(),
            )))
            .as_bytes(),
            binding_key(extraction_key(ContentHash::compute_typed(
                "blob",
                source.source().as_bytes(),
            )))
            .as_bytes(),
        ])
        .to_hex();
        if ambiguous {
            change.limitations.push(Limitation::AmbiguousMatch);
            push(
                &mut change,
                old_ids,
                new_ids,
                CorrespondenceKind::Ambiguous,
                vec![],
            );
        } else if change.limitations.iter().any(|l| {
            matches!(
                l,
                Limitation::Syntax
                    | Limitation::MutableBinding
                    | Limitation::MissingElse
                    | Limitation::Budget
            )
        }) {
            push(
                &mut change,
                old_ids,
                new_ids,
                CorrespondenceKind::Unmatched,
                vec![],
            );
        } else {
            correspond(&mut change, old_ids.first(), new_ids.first());
        }
        if !change.limitations.is_empty() {
            change.support = Support::Partial;
        }
        scope.support = change.support;
        scope.limitations = change.limitations.clone();
        out.analyzed.push(scope);
        out.changes.push(change);
    }
}

fn omit(out: &mut Comparison, reason: Limitation) {
    out.exhausted = !matches!(reason, Limitation::Budget | Limitation::MissingSource);
    out.omitted.push(Scope {
        symbol_address: String::new(),
        target: String::new(),
        support: if matches!(reason, Limitation::MissingSource | Limitation::Budget) {
            Support::Unavailable
        } else {
            Support::Unsupported
        },
        limitations: vec![reason],
        sources: vec![],
    });
}

fn expression<'a>(change: &'a Change, id: &str) -> Option<&'a Expression> {
    change.expressions.iter().find(|e| e.id == id)
}

fn resolved_hash(change: &Change, id: &str) -> Option<ContentHash> {
    let mut current = id;
    for _ in 0..=16 {
        let binding = change.bindings.iter().find(|b| b.occurrence_id == current);
        match binding {
            Some(binding) if binding.support == Support::Supported => {
                current = binding.value_id.as_deref()?;
            }
            Some(binding) if binding.limitations != [Limitation::UnresolvedBinding] => return None,
            _ => return expression(change, current).map(|e| e.normalized_hash),
        }
    }
    None
}

fn equivalent(a: &Change, b: &Change) -> bool {
    let (Some(a_value), Some(b_value)) = (
        a.assignments
            .first()
            .and_then(|v| expression(a, &v.value_id)),
        b.assignments
            .first()
            .and_then(|v| expression(b, &v.value_id)),
    ) else {
        return false;
    };
    if a_value.normalized_hash != b_value.normalized_hash {
        return false;
    }
    match (&a_value.conditional, &b_value.conditional) {
        (Some(ac), Some(bc)) => {
            let (ah, bh) = (
                predicate_hash(a, &ac.predicate_id),
                predicate_hash(b, &bc.predicate_id),
            );
            ah.is_some() && ah == bh
        }
        (None, None) => true,
        _ => false,
    }
}

fn predicate_hash(change: &Change, id: &str) -> Option<ContentHash> {
    let predicate = expression(change, id)?;
    if change.bindings.iter().any(|b| b.occurrence_id == id) {
        return resolved_hash(change, id);
    }
    let mut hashes = vec![predicate.normalized_hash];
    for binding in &change.bindings {
        let occurrence = expression(change, &binding.occurrence_id)?;
        if occurrence.source.side == predicate.source.side
            && occurrence.source.span.start >= predicate.source.span.start
            && occurrence.source.span.end <= predicate.source.span.end
        {
            hashes.push(resolved_hash(change, &binding.occurrence_id)?);
        }
    }
    Some(extract::hash(
        &hashes
            .iter()
            .map(|h| h.as_bytes().as_slice())
            .collect::<Vec<_>>(),
    ))
}

fn push(
    change: &mut Change,
    base_ids: Vec<String>,
    source_ids: Vec<String>,
    kind: CorrespondenceKind,
    reasons: Vec<MatchReason>,
) {
    let id = extract::hash(&[
        change.id.as_bytes(),
        b"correspondence",
        &(change.correspondences.len() as u64).to_le_bytes(),
    ])
    .to_hex();
    change.correspondences.push(Correspondence {
        id,
        base_ids,
        source_ids,
        kind,
        reasons,
    });
}

fn pair(change: &mut Change, old: &str, new: &str, predicate: bool, branch: bool) {
    let a = expression(change, old).map(|e| e.normalized_hash);
    let b = expression(change, new).map(|e| e.normalized_hash);
    let (a, b) = if predicate {
        (predicate_hash(change, old), predicate_hash(change, new))
    } else {
        (a, b)
    };
    let mut reasons = vec![MatchReason::SameTargetInMatchedSymbol];
    if branch {
        reasons.push(MatchReason::SameBranchLabel);
    }
    let kind = match (a, b) {
        (Some(a), Some(b)) if a == b => {
            reasons.push(if predicate {
                MatchReason::ResolvedBinding
            } else {
                MatchReason::ExactNormalizedExpression
            });
            CorrespondenceKind::Retained
        }
        (Some(_), Some(_)) => CorrespondenceKind::Replaced,
        _ => CorrespondenceKind::Unmatched,
    };
    push(change, vec![old.into()], vec![new.into()], kind, reasons);
}

fn correspond(change: &mut Change, old: Option<&String>, new: Option<&String>) {
    match (old, new) {
        (Some(old), Some(new)) => {
            let a = expression(change, old).and_then(|e| e.conditional.clone());
            let b = expression(change, new).and_then(|e| e.conditional.clone());
            match (a, b) {
                (None, Some(b)) => {
                    for branch in b.branches {
                        pair(change, old, &branch.result_id, false, false);
                    }
                }
                (Some(a), None) => {
                    for branch in a.branches {
                        pair(change, &branch.result_id, new, false, false);
                    }
                }
                (Some(a), Some(b)) => {
                    pair(change, &a.predicate_id, &b.predicate_id, true, false);
                    for (a, b) in a.branches.iter().zip(&b.branches) {
                        pair(change, &a.result_id, &b.result_id, false, true);
                    }
                }
                (None, None) => pair(change, old, new, false, false),
            }
        }
        (None, Some(new)) => push(
            change,
            vec![],
            vec![new.clone()],
            CorrespondenceKind::Added,
            vec![],
        ),
        (Some(old), None) => push(
            change,
            vec![old.clone()],
            vec![],
            CorrespondenceKind::Removed,
            vec![],
        ),
        (None, None) => {}
    }
}
