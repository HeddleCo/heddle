// SPDX-License-Identifier: Apache-2.0
//! Similarity computation utilities.

use std::collections::{HashMap, HashSet};

use crate::parser::{Language, ParsedFile};

/// Method for computing content similarity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimilarityMethod {
    /// Simple line-by-line comparison.
    Lines,
    /// Token-based comparison (ignores whitespace).
    Tokens,
    /// AST-based comparison (structure only).
    Ast,
}

/// Compute similarity between two strings (0.0 to 1.0).
pub fn compute_similarity(a: &str, b: &str, method: SimilarityMethod) -> f64 {
    match method {
        SimilarityMethod::Lines => {
            let lines_a: HashSet<&str> = a.lines().filter(|l| !l.trim().is_empty()).collect();
            let lines_b: HashSet<&str> = b.lines().filter(|l| !l.trim().is_empty()).collect();

            if lines_a.is_empty() && lines_b.is_empty() {
                return 1.0;
            }
            if lines_a.is_empty() || lines_b.is_empty() {
                return 0.0;
            }

            let intersection: HashSet<_> = lines_a.intersection(&lines_b).collect();
            let union: HashSet<_> = lines_a.union(&lines_b).collect();

            let line_similarity = intersection.len() as f64 / union.len() as f64;
            if line_similarity == 0.0 {
                return compute_similarity(a, b, SimilarityMethod::Tokens);
            }

            line_similarity
        }
        SimilarityMethod::Tokens => {
            let tokens_a: HashSet<&str> = a.split_whitespace().collect();
            let tokens_b: HashSet<&str> = b.split_whitespace().collect();

            if tokens_a.is_empty() && tokens_b.is_empty() {
                return 1.0;
            }
            if tokens_a.is_empty() || tokens_b.is_empty() {
                return 0.0;
            }

            let intersection: HashSet<_> = tokens_a.intersection(&tokens_b).collect();
            let union: HashSet<_> = tokens_a.union(&tokens_b).collect();

            intersection.len() as f64 / union.len() as f64
        }
        // AST similarity is language-dependent: without a grammar there is no
        // tree to compare. The language-free entry point therefore cannot
        // honor `Ast` itself — it forwards to the one sanctioned AST path,
        // which degrades to token similarity for `Language::Unknown` rather
        // than silently masquerading token similarity as an AST result.
        SimilarityMethod::Ast => {
            compute_similarity_with_language(a, b, SimilarityMethod::Ast, Language::Unknown)
        }
    }
}

pub fn compute_similarity_with_language(
    a: &str,
    b: &str,
    method: SimilarityMethod,
    language: Language,
) -> f64 {
    match method {
        SimilarityMethod::Ast => {
            if let Some(score) = compute_ast_similarity(a, b, language) {
                return score;
            }
            compute_similarity(a, b, SimilarityMethod::Tokens)
        }
        _ => compute_similarity(a, b, method),
    }
}

/// AST kind-bag similarity with no token fallback.
///
/// `None` means a language has no grammar or either side failed to
/// parse. Callers that must not invent a novelty/uniqueness signal
/// from identifier tokens should treat that as fail-closed.
pub fn try_compute_ast_similarity(a: &str, b: &str, language: Language) -> Option<f64> {
    try_compute_ast_similarity_for_languages(a, language, b, language)
}

/// Like [`try_compute_ast_similarity`], but each side uses its own grammar.
pub fn try_compute_ast_similarity_for_languages(
    a: &str,
    a_language: Language,
    b: &str,
    b_language: Language,
) -> Option<f64> {
    if a_language == Language::Unknown || b_language == Language::Unknown {
        return None;
    }
    let counts_a = ast_node_counts(a, a_language)?;
    let counts_b = ast_node_counts(b, b_language)?;
    Some(count_similarity(&counts_a, &counts_b))
}

pub(super) struct PreparedSimilarity {
    method: SimilarityMethod,
    lines: Option<HashSet<String>>,
    tokens: HashSet<String>,
    ast_counts: HashMap<Language, Option<HashMap<String, usize>>>,
}

impl PreparedSimilarity {
    pub(super) fn new(
        content: &str,
        method: SimilarityMethod,
        languages: impl IntoIterator<Item = Language>,
    ) -> Self {
        let tokens = content
            .split_whitespace()
            .map(String::from)
            .collect::<HashSet<_>>();
        let lines = (method == SimilarityMethod::Lines).then(|| {
            content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(String::from)
                .collect()
        });
        let ast_counts = if method == SimilarityMethod::Ast {
            languages
                .into_iter()
                .map(|language| (language, ast_node_counts(content, language)))
                .collect()
        } else {
            HashMap::new()
        };
        Self {
            method,
            lines,
            tokens,
            ast_counts,
        }
    }

    pub(super) fn similarity(&self, other: &Self, language: Language) -> f64 {
        debug_assert_eq!(self.method, other.method);
        match self.method {
            SimilarityMethod::Lines => {
                let score = set_similarity(
                    self.lines.as_ref().expect("lines prepared"),
                    other.lines.as_ref().expect("lines prepared"),
                );
                if score == 0.0 {
                    set_similarity(&self.tokens, &other.tokens)
                } else {
                    score
                }
            }
            SimilarityMethod::Tokens => set_similarity(&self.tokens, &other.tokens),
            SimilarityMethod::Ast => match (
                self.ast_counts.get(&language).and_then(Option::as_ref),
                other.ast_counts.get(&language).and_then(Option::as_ref),
            ) {
                (Some(left), Some(right)) => count_similarity(left, right),
                _ => set_similarity(&self.tokens, &other.tokens),
            },
        }
    }
}

fn set_similarity(left: &HashSet<String>, right: &HashSet<String>) -> f64 {
    if left.is_empty() && right.is_empty() {
        return 1.0;
    }
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    left.intersection(right).count() as f64 / left.union(right).count() as f64
}

fn count_similarity(left: &HashMap<String, usize>, right: &HashMap<String, usize>) -> f64 {
    if left.is_empty() && right.is_empty() {
        return 1.0;
    }
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let keys = left.keys().chain(right.keys()).collect::<HashSet<_>>();
    let (intersection, union) = keys.into_iter().fold((0usize, 0usize), |totals, key| {
        let left_count = left.get(key).copied().unwrap_or(0);
        let right_count = right.get(key).copied().unwrap_or(0);
        (
            totals.0 + left_count.min(right_count),
            totals.1 + left_count.max(right_count),
        )
    });
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

fn ast_node_counts(content: &str, language: Language) -> Option<HashMap<String, usize>> {
    let parsed = ParsedFile::parse(content, language)?;
    let mut counts = HashMap::new();
    collect_node_kinds(parsed.root_node(), &mut counts);
    Some(counts)
}

fn compute_ast_similarity(a: &str, b: &str, language: Language) -> Option<f64> {
    let counts_a = ast_node_counts(a, language)?;
    let counts_b = ast_node_counts(b, language)?;
    Some(count_similarity(&counts_a, &counts_b))
}

fn collect_node_kinds(node: tree_sitter::Node<'_>, counts: &mut HashMap<String, usize>) {
    let mut stack = vec![node];

    while let Some(current) = stack.pop() {
        let kind = current.kind();
        let entry = counts.entry(kind.to_string()).or_insert(0);
        *entry += 1;

        let child_count = current.child_count();
        for index in (0..child_count).rev() {
            if let Some(child) = current.child(index as u32) {
                stack.push(child);
            }
        }
    }
}
