// SPDX-License-Identifier: Apache-2.0
//! Function-level semantic changes.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use objects::object::{ChangeImportance, SemanticChange};

use super::analysis_similarity::{SimilarityMethod, compute_similarity_with_language};
use crate::parser::{FunctionDef, Language, ParsedFile};

const FUNCTION_RENAME_SIMILARITY_THRESHOLD: f64 = 0.58;

/// Identity used for cross-version function matching. The bare name
/// would collapse same-name redeclarations — JavaScript permits two
/// `function foo()` statements at module scope, Python permits
/// repeated top-level `def foo()`, and `BTreeMap<String, FunctionDef>`
/// would silently keep only the last occurrence per side. `FunctionKey`
/// pairs the name with the function's per-name occurrence index within
/// its side (0 for the first, 1 for the second, …) so each duplicate
/// gets a distinct slot. Cross-version matching is then positional:
/// old's first `foo` pairs with new's first `foo`. Mirrors the
/// merge_driver pattern (commit 2198b00, heddle#114 r5 P1 #2).
type FunctionKey = (String, usize);

fn build_function_map(parsed: Option<&ParsedFile>) -> BTreeMap<FunctionKey, FunctionDef> {
    let Some(parsed) = parsed else {
        return BTreeMap::new();
    };
    let mut counters: BTreeMap<String, usize> = BTreeMap::new();
    let mut map = BTreeMap::new();
    for func in parsed.extract_functions() {
        let n = counters.entry(func.name.clone()).or_insert(0);
        let occurrence = *n;
        *n += 1;
        map.insert((func.name.clone(), occurrence), func);
    }
    map
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

    let removed_old_keys: BTreeSet<FunctionKey> = old_funcs
        .keys()
        .filter(|k| !new_funcs.contains_key(*k))
        .cloned()
        .collect();
    let moved_function_names = stable_order_moved_names(&old_funcs, &new_funcs);
    let mut matched_old_keys: HashSet<FunctionKey> = HashSet::new();

    for (new_key, func) in &new_funcs {
        let name = &new_key.0;
        if !old_funcs.contains_key(new_key) {
            let renamed_from = removed_old_keys
                .iter()
                .filter(|old_key| !matched_old_keys.contains(*old_key))
                .filter_map(|old_key| {
                    let old_name = &old_key.0;
                    let old_func = old_funcs.get(old_key)?;
                    let similarity = compute_similarity_with_language(
                        &normalized_function_for_matching(&old_func.content, old_name),
                        &normalized_function_for_matching(&func.content, name),
                        similarity_method,
                        Language::from_path(new_path),
                    );

                    let same_location_update = old_path == new_path
                        && old_func.start_line.abs_diff(func.start_line) <= 5
                        && similarity >= 0.30;
                    (similarity >= FUNCTION_RENAME_SIMILARITY_THRESHOLD || same_location_update)
                        .then_some((old_key, similarity))
                })
                .max_by(|(left_key, left_similarity), (right_key, right_similarity)| {
                    left_similarity
                        .total_cmp(right_similarity)
                        .then_with(|| right_key.cmp(left_key))
                })
                .map(|(old_key, _)| old_key.clone());

            if let Some(old_key) = renamed_from {
                let old_name = old_key.0.clone();
                matched_old_keys.insert(old_key);
                changes.push(SemanticChange::FunctionRenamed {
                    file: new_path.to_path_buf(),
                    old_name,
                    new_name: name.clone(),
                    importance: Some(ChangeImportance::Low),
                });
                file_modified = true;
            } else {
                let source = extraction_source(&old_funcs, func);
                if let Some(source_name) = source {
                    changes.push(SemanticChange::FunctionExtracted {
                        file: new_path.to_path_buf(),
                        name: name.clone(),
                        source_file: Some(old_path.to_path_buf()),
                        source_name: Some(source_name),
                        importance: Some(ChangeImportance::High),
                    });
                } else {
                    changes.push(SemanticChange::FunctionAdded {
                        file: new_path.to_path_buf(),
                        name: name.clone(),
                        importance: Some(ChangeImportance::High),
                    });
                }
                file_modified = true;
            }
        }
    }

    for old_key in removed_old_keys {
        if matched_old_keys.contains(&old_key) {
            continue;
        }
        changes.push(SemanticChange::FunctionDeleted {
            file: new_path.to_path_buf(),
            name: old_key.0,
            importance: Some(ChangeImportance::High),
        });
        file_modified = true;
    }

    for (new_key, new_func) in &new_funcs {
        let name = &new_key.0;
        if let Some(old_func) = old_funcs.get(new_key)
            && old_func.signature != new_func.signature
        {
            changes.push(SemanticChange::SignatureChanged {
                file: new_path.to_path_buf(),
                name: name.clone(),
                old_signature: old_func.signature.clone(),
                new_signature: new_func.signature.clone(),
                importance: Some(ChangeImportance::Medium),
            });
            file_modified = true;
        } else if let Some(old_func) = old_funcs.get(new_key) {
            if old_path == new_path
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

fn extraction_source(
    old_funcs: &BTreeMap<FunctionKey, FunctionDef>,
    extracted: &FunctionDef,
) -> Option<String> {
    let extracted_lines = meaningful_body_lines(&extracted.content);
    if extracted_lines.is_empty() {
        return None;
    }

    old_funcs
        .iter()
        .filter_map(|(key, old_func)| {
            let old_lines = meaningful_body_lines(&old_func.content);
            let evidence = extraction_evidence(&old_lines, &extracted_lines);
            evidence.is_strong().then_some((key.0.clone(), evidence))
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
    old_funcs: &BTreeMap<FunctionKey, FunctionDef>,
    new_funcs: &BTreeMap<FunctionKey, FunctionDef>,
) -> HashSet<String> {
    let mut old_order = stable_function_order(old_funcs, new_funcs, true);
    let mut new_order = stable_function_order(old_funcs, new_funcs, false);

    if old_order == new_order {
        return HashSet::new();
    }

    old_order
        .drain(..)
        .zip(new_order.drain(..))
        .filter_map(|(old_name, new_name)| (old_name != new_name).then_some([old_name, new_name]))
        .flatten()
        .collect()
}

fn stable_function_order(
    old_funcs: &BTreeMap<FunctionKey, FunctionDef>,
    new_funcs: &BTreeMap<FunctionKey, FunctionDef>,
    use_old_position: bool,
) -> Vec<String> {
    let mut ordered = old_funcs
        .iter()
        .filter_map(|(key, old_func)| {
            let new_func = new_funcs.get(key)?;
            (old_func.content == new_func.content).then_some((
                if use_old_position {
                    old_func.start_line
                } else {
                    new_func.start_line
                },
                key.0.clone(),
            ))
        })
        .collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    ordered.into_iter().map(|(_, name)| name).collect()
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