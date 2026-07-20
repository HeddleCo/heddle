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

fn compute_ast_similarity(a: &str, b: &str, language: Language) -> Option<f64> {
    let parsed_a = ParsedFile::parse(a, language)?;
    let parsed_b = ParsedFile::parse(b, language)?;

    let mut counts_a = HashMap::new();
    let mut counts_b = HashMap::new();

    collect_node_kinds(parsed_a.root_node(), &mut counts_a);
    collect_node_kinds(parsed_b.root_node(), &mut counts_b);

    if counts_a.is_empty() && counts_b.is_empty() {
        return Some(1.0);
    }
    if counts_a.is_empty() || counts_b.is_empty() {
        return Some(0.0);
    }

    let mut intersection = 0usize;
    let mut union = 0usize;
    let mut keys: HashSet<&str> = HashSet::new();
    keys.extend(counts_a.keys().map(|k| k.as_str()));
    keys.extend(counts_b.keys().map(|k| k.as_str()));

    for key in keys {
        let count_a = counts_a.get(key).copied().unwrap_or(0);
        let count_b = counts_b.get(key).copied().unwrap_or(0);
        intersection += count_a.min(count_b);
        union += count_a.max(count_b);
    }

    if union == 0 {
        Some(0.0)
    } else {
        Some(intersection as f64 / union as f64)
    }
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
