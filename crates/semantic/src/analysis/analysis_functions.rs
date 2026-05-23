// SPDX-License-Identifier: Apache-2.0
//! Function-level semantic changes.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use objects::object::{ChangeImportance, SemanticChange};

use super::analysis_similarity::{SimilarityMethod, compute_similarity_with_language};
use crate::parser::{FunctionDef, Language, ParsedFile};

const FUNCTION_RENAME_SIMILARITY_THRESHOLD: f64 = 0.58;

/// Multi-map: function name → all definitions on this side with that
/// name, in source order.
///
/// `BTreeMap<String, FunctionDef>` would silently collapse same-name
/// redeclarations (JS allows two `function foo()` at module scope;
/// Python allows repeated top-level `def foo()`). A prior fix (r1)
/// keyed entries by `(name, occurrence)` to stop the collapse, but
/// that paired old's `foo[0]` with new's `foo[0]` regardless of body
/// content — so a fresh same-name definition inserted before existing
/// ones produced a bogus FunctionModified with the wrong delta plus a
/// misclassified add/delete (Codex cid 3259311747, heddle#125 r2).
///
/// Keeping a `Vec` per name lets us pair instances across versions by
/// *content similarity* (see `pair_within_name`) rather than by
/// within-side position. merge_driver keeps the positional keying
/// (commit 2198b00) because its job is line-up merging within a
/// logical slot; analysis_functions needs fuzzy cross-version identity.
type FunctionMap = BTreeMap<String, Vec<FunctionDef>>;

/// `(name, within-side index)` — identifies a single function instance
/// on one side. Not a cross-version identity.
type InstanceRef = (String, usize);

fn build_function_map(parsed: Option<&ParsedFile>) -> FunctionMap {
    let Some(parsed) = parsed else {
        return BTreeMap::new();
    };
    let mut map = FunctionMap::new();
    for func in parsed.extract_functions() {
        map.entry(func.name.clone()).or_default().push(func);
    }
    map
}

/// Greedy best-similarity matcher within a single name bucket. Returns
/// (paired old/new indices, unpaired old indices, unpaired new indices).
fn pair_within_name(
    olds: &[FunctionDef],
    news: &[FunctionDef],
    similarity_method: SimilarityMethod,
    language: Language,
) -> (Vec<(usize, usize)>, Vec<usize>, Vec<usize>) {
    let mut candidates: Vec<(usize, usize, f64)> = Vec::with_capacity(olds.len() * news.len());
    for (i, o) in olds.iter().enumerate() {
        for (j, n) in news.iter().enumerate() {
            let similarity = if o.content == n.content {
                1.0
            } else {
                compute_similarity_with_language(
                    &o.content,
                    &n.content,
                    similarity_method,
                    language,
                )
            };
            candidates.push((i, j, similarity));
        }
    }
    // Highest similarity first; deterministic tiebreak: lowest old, then lowest new.
    candidates
        .sort_by(|(li, lj, ls), (ri, rj, rs)| rs.total_cmp(ls).then(li.cmp(ri)).then(lj.cmp(rj)));

    let mut old_used = vec![false; olds.len()];
    let mut new_used = vec![false; news.len()];
    let mut pairs = Vec::new();
    for (i, j, _) in candidates {
        if !old_used[i] && !new_used[j] {
            old_used[i] = true;
            new_used[j] = true;
            pairs.push((i, j));
        }
    }
    let unmatched_old = old_used
        .iter()
        .enumerate()
        .filter_map(|(i, used)| (!*used).then_some(i))
        .collect();
    let unmatched_new = new_used
        .iter()
        .enumerate()
        .filter_map(|(j, used)| (!*used).then_some(j))
        .collect();
    (pairs, unmatched_old, unmatched_new)
}

/// Detect function-level changes between two file versions.
pub fn detect_function_changes(
    old_path: &std::path::Path,
    new_path: &std::path::Path,
    old_content: &str,
    new_content: &str,
    similarity_method: SimilarityMethod,
) -> Vec<SemanticChange> {
    let old_parsed = ParsedFile::parse(old_content, Language::from_path(old_path));
    let new_parsed = ParsedFile::parse(new_content, Language::from_path(new_path));

    detect_function_changes_with_parsed(
        old_path,
        new_path,
        old_parsed.as_ref(),
        new_parsed.as_ref(),
        similarity_method,
    )
}

pub(crate) fn detect_function_changes_with_parsed(
    old_path: &std::path::Path,
    new_path: &std::path::Path,
    old_parsed: Option<&ParsedFile>,
    new_parsed: Option<&ParsedFile>,
    similarity_method: SimilarityMethod,
) -> Vec<SemanticChange> {
    let mut changes = Vec::new();
    let mut file_modified = false;

    let old_funcs = build_function_map(old_parsed);
    let new_funcs = build_function_map(new_parsed);
    let language = Language::from_path(new_path);

    // Phase 1: pair instances within each name bucket by content similarity.
    let mut pairs: Vec<(String, usize, usize)> = Vec::new();
    let mut unmatched_old: Vec<InstanceRef> = Vec::new();
    let mut unmatched_new: Vec<InstanceRef> = Vec::new();

    let mut all_names: BTreeSet<&str> = BTreeSet::new();
    all_names.extend(old_funcs.keys().map(String::as_str));
    all_names.extend(new_funcs.keys().map(String::as_str));

    let empty: Vec<FunctionDef> = Vec::new();
    for name in &all_names {
        let olds = old_funcs.get(*name).unwrap_or(&empty);
        let news = new_funcs.get(*name).unwrap_or(&empty);
        let (within, u_old, u_new) = pair_within_name(olds, news, similarity_method, language);
        for (oi, ni) in within {
            pairs.push(((*name).to_string(), oi, ni));
        }
        unmatched_old.extend(u_old.into_iter().map(|i| ((*name).to_string(), i)));
        unmatched_new.extend(u_new.into_iter().map(|i| ((*name).to_string(), i)));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));

    let moved_function_names = stable_order_moved_names(&old_funcs, &new_funcs, &pairs);

    // Phase 2: cross-name rename detection over leftovers.
    let mut consumed_old: HashSet<InstanceRef> = HashSet::new();
    for (new_name, ni) in &unmatched_new {
        let new_func = &new_funcs[new_name][*ni];
        let renamed_from = unmatched_old
            .iter()
            .filter(|(on, oi)| !consumed_old.contains(&(on.clone(), *oi)))
            .filter(|(on, _)| on != new_name)
            .filter_map(|(on, oi)| {
                let old_func = &old_funcs[on][*oi];
                let similarity = compute_similarity_with_language(
                    &normalized_function_for_matching(&old_func.content, on),
                    &normalized_function_for_matching(&new_func.content, new_name),
                    similarity_method,
                    language,
                );
                let same_location_update = old_path == new_path
                    && old_func.start_line.abs_diff(new_func.start_line) <= 5
                    && similarity >= 0.30;
                (similarity >= FUNCTION_RENAME_SIMILARITY_THRESHOLD || same_location_update)
                    .then_some(((on.clone(), *oi), similarity))
            })
            .max_by(
                |(left_key, left_similarity), (right_key, right_similarity)| {
                    left_similarity
                        .total_cmp(right_similarity)
                        .then_with(|| right_key.cmp(left_key))
                },
            )
            .map(|(key, _)| key);

        if let Some((old_name, old_idx)) = renamed_from {
            consumed_old.insert((old_name.clone(), old_idx));
            changes.push(SemanticChange::FunctionRenamed {
                file: new_path.to_path_buf(),
                old_name,
                new_name: new_name.clone(),
                importance: Some(ChangeImportance::Low),
            });
            file_modified = true;
        } else {
            let source = extraction_source(&old_funcs, new_func);
            if let Some(source_name) = source {
                changes.push(SemanticChange::FunctionExtracted {
                    file: new_path.to_path_buf(),
                    name: new_name.clone(),
                    source_file: Some(old_path.to_path_buf()),
                    source_name: Some(source_name),
                    importance: Some(ChangeImportance::High),
                });
            } else {
                changes.push(SemanticChange::FunctionAdded {
                    file: new_path.to_path_buf(),
                    name: new_name.clone(),
                    importance: Some(ChangeImportance::High),
                });
            }
            file_modified = true;
        }
    }

    // Phase 3: unconsumed unmatched_old → deletions.
    for (old_name, oi) in &unmatched_old {
        if consumed_old.contains(&(old_name.clone(), *oi)) {
            continue;
        }
        changes.push(SemanticChange::FunctionDeleted {
            file: new_path.to_path_buf(),
            name: old_name.clone(),
            importance: Some(ChangeImportance::High),
        });
        file_modified = true;
    }

    // Phase 4: paired instances → signature / move / modified.
    for (name, oi, ni) in &pairs {
        let old_func = &old_funcs[name][*oi];
        let new_func = &new_funcs[name][*ni];
        if old_func.signature != new_func.signature {
            changes.push(SemanticChange::SignatureChanged {
                file: new_path.to_path_buf(),
                name: name.clone(),
                old_signature: old_func.signature.clone(),
                new_signature: new_func.signature.clone(),
                importance: Some(ChangeImportance::Medium),
            });
            file_modified = true;
        } else if old_path == new_path
            && old_func.content == new_func.content
            && old_func.start_line != new_func.start_line
            && moved_function_names.contains(name)
        {
            changes.push(SemanticChange::FunctionMoved {
                file: new_path.to_path_buf(),
                name: name.clone(),
                old_start_line: old_func.start_line,
                new_start_line: new_func.start_line,
                importance: Some(ChangeImportance::Low),
            });
            file_modified = true;
        } else if old_func.content != new_func.content {
            changes.push(SemanticChange::FunctionModified {
                file: new_path.to_path_buf(),
                name: name.clone(),
                importance: Some(ChangeImportance::Medium),
            });
            file_modified = true;
        }
    }

    if file_modified {
        changes.push(SemanticChange::FileModified {
            path: new_path.to_path_buf(),
            classification: None,
            importance: None,
            confidence: None,
        });
    }

    changes
}

fn extraction_source(old_funcs: &FunctionMap, extracted: &FunctionDef) -> Option<String> {
    let extracted_lines = meaningful_body_lines(&extracted.content);
    if extracted_lines.is_empty() {
        return None;
    }

    old_funcs
        .iter()
        .flat_map(|(name, funcs)| funcs.iter().map(move |func| (name.clone(), func)))
        .filter_map(|(name, old_func)| {
            let old_lines = meaningful_body_lines(&old_func.content);
            let evidence = extraction_evidence(&old_lines, &extracted_lines);
            evidence.is_strong().then_some((name, evidence))
        })
        .max_by(|left, right| {
            left.1
                .score
                .total_cmp(&right.1.score)
                .then_with(|| left.1.matched.cmp(&right.1.matched))
                .then_with(|| right.0.cmp(&left.0))
        })
        .map(|(name, _)| name)
}

#[derive(Debug)]
struct ExtractionEvidence {
    matched: usize,
    score: f64,
    exact_matches: usize,
    longest_exact_expression_len: usize,
    extracted_lines: usize,
}

impl ExtractionEvidence {
    fn is_strong(&self) -> bool {
        if self.extracted_lines == 0 {
            return false;
        }

        let coverage = self.matched as f64 / self.extracted_lines as f64;
        let weighted_coverage = self.score / self.extracted_lines as f64;

        if self.extracted_lines == 1 {
            return self.exact_matches == 1
                && weighted_coverage >= 0.95
                && self.longest_exact_expression_len >= 20;
        }

        coverage >= 0.70 && weighted_coverage >= 0.70
    }
}

fn extraction_evidence(old_lines: &[String], extracted_lines: &[String]) -> ExtractionEvidence {
    let mut matched = 0;
    let mut score = 0.0;
    let mut exact_matches = 0;
    let mut longest_exact_expression_len = 0;

    for line in extracted_lines {
        let best = old_lines
            .iter()
            .map(|old_line| body_line_match(old_line, line))
            .max_by(|left, right| left.score.total_cmp(&right.score))
            .unwrap_or_default();
        if best.score > 0.0 {
            matched += 1;
            score += best.score;
        }
        if best.score >= 1.0 {
            exact_matches += 1;
            longest_exact_expression_len = longest_exact_expression_len.max(best.expression_len);
        }
    }

    ExtractionEvidence {
        matched,
        score,
        exact_matches,
        longest_exact_expression_len,
        extracted_lines: extracted_lines.len(),
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct BodyLineMatch {
    score: f64,
    expression_len: usize,
}

fn body_line_match(old_line: &str, extracted_line: &str) -> BodyLineMatch {
    let old = comparable_body_expression(old_line);
    let extracted = comparable_body_expression(extracted_line);
    if old == extracted {
        return BodyLineMatch {
            score: 1.0,
            expression_len: extracted.len(),
        };
    }
    if extracted.len() >= 24 && old.contains(&extracted) {
        return BodyLineMatch {
            score: 0.75,
            expression_len: extracted.len(),
        };
    }
    if old.len() >= 24 && extracted.contains(&old) {
        return BodyLineMatch {
            score: 0.75,
            expression_len: old.len(),
        };
    }
    BodyLineMatch::default()
}

fn comparable_body_expression(line: &str) -> String {
    let trimmed = line
        .trim()
        .trim_end_matches(';')
        .trim_start_matches("return ")
        .trim();
    let expression = trimmed
        .split_once('=')
        .map(|(_, rhs)| rhs.trim())
        .unwrap_or(trimmed);
    expression.trim_end_matches(';').trim().to_string()
}

fn meaningful_body_lines(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && *line != "{"
                && *line != "}"
                && !line.starts_with("fn ")
                && !line.starts_with("pub fn ")
                && !line.starts_with("async fn ")
                && !line.starts_with("pub async fn ")
        })
        .map(ToString::to_string)
        .collect()
}

fn stable_order_moved_names(
    old_funcs: &FunctionMap,
    new_funcs: &FunctionMap,
    pairs: &[(String, usize, usize)],
) -> HashSet<String> {
    let mut old_order: Vec<(usize, String)> = Vec::new();
    let mut new_order: Vec<(usize, String)> = Vec::new();
    for (name, oi, ni) in pairs {
        let old_func = &old_funcs[name][*oi];
        let new_func = &new_funcs[name][*ni];
        if old_func.content == new_func.content {
            old_order.push((old_func.start_line, name.clone()));
            new_order.push((new_func.start_line, name.clone()));
        }
    }
    old_order.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    new_order.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    let old_names: Vec<String> = old_order.into_iter().map(|(_, n)| n).collect();
    let new_names: Vec<String> = new_order.into_iter().map(|(_, n)| n).collect();
    if old_names == new_names {
        return HashSet::new();
    }
    old_names
        .into_iter()
        .zip(new_names)
        .filter_map(|(old_name, new_name)| (old_name != new_name).then_some([old_name, new_name]))
        .flatten()
        .collect()
}

fn normalized_function_for_matching(content: &str, name: &str) -> String {
    content
        .replace(name, "__function_name__")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
