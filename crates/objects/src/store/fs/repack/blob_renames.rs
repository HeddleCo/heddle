// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashSet, path::Path};

const RENAME_THRESHOLD: f64 = 0.6;

pub(super) fn similarity_renames(
    deleted: &[(String, String)],
    added: &[(String, String)],
) -> Vec<(usize, usize)> {
    let deleted_prepared = deleted
        .iter()
        .map(|(_, content)| PreparedText::new(content))
        .collect::<Vec<_>>();
    let added_prepared = added
        .iter()
        .map(|(_, content)| PreparedText::new(content))
        .collect::<Vec<_>>();
    let mut candidates = Vec::new();
    for (source, old) in deleted_prepared.iter().enumerate() {
        for (target, new) in added_prepared.iter().enumerate() {
            let old_language = language_family(&deleted[source].0);
            let new_language = language_family(&added[target].0);
            if old_language.is_some() && new_language.is_some() && old_language != new_language {
                continue;
            }
            let score = old.similarity(new);
            if score >= RENAME_THRESHOLD {
                candidates.push(Candidate {
                    source,
                    target,
                    score,
                });
            }
        }
    }
    candidates.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.source.cmp(&right.source))
            .then_with(|| left.target.cmp(&right.target))
    });
    let mut used_sources = vec![false; deleted.len()];
    let mut used_targets = vec![false; added.len()];
    candidates
        .into_iter()
        .filter_map(|candidate| {
            if used_sources[candidate.source] || used_targets[candidate.target] {
                return None;
            }
            used_sources[candidate.source] = true;
            used_targets[candidate.target] = true;
            Some((candidate.source, candidate.target))
        })
        .collect()
}

fn language_family(path: &str) -> Option<&'static str> {
    match Path::new(path).extension().and_then(|value| value.to_str()) {
        Some("rs") => Some("rust"),
        Some("py" | "pyi") => Some("python"),
        Some("js" | "jsx" | "mjs" | "cjs") => Some("javascript"),
        Some("ts" | "tsx") => Some("typescript"),
        Some("go") => Some("go"),
        Some("c" | "h") => Some("c"),
        Some("cpp" | "cc" | "hpp" | "cxx") => Some("cpp"),
        Some("java") => Some("java"),
        Some("zig") => Some("zig"),
        _ => None,
    }
}

struct Candidate {
    source: usize,
    target: usize,
    score: f64,
}

struct PreparedText {
    lines: HashSet<String>,
    tokens: HashSet<String>,
}

impl PreparedText {
    fn new(content: &str) -> Self {
        Self {
            lines: content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(String::from)
                .collect(),
            tokens: content.split_whitespace().map(String::from).collect(),
        }
    }

    fn similarity(&self, other: &Self) -> f64 {
        let lines = set_similarity(&self.lines, &other.lines);
        if lines == 0.0 {
            set_similarity(&self.tokens, &other.tokens)
        } else {
            lines
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

#[cfg(test)]
mod tests {
    use super::similarity_renames;

    #[test]
    fn rename_assignment_is_deterministic_and_one_to_one() {
        let deleted = vec![
            ("old-a".into(), "same\nbody\nkept\nstable\n".into()),
            ("old-b".into(), "same\nbody\nkept\nstable\n".into()),
        ];
        let added = vec![
            ("new-a".into(), "same\nbody\nkept\nchanged\n".into()),
            ("new-b".into(), "same\nbody\nkept\nchanged\n".into()),
        ];

        assert_eq!(similarity_renames(&deleted, &added), vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn known_different_languages_are_not_rename_candidates() {
        let deleted = vec![("old.rs".into(), "same content".into())];
        let added = vec![("new.py".into(), "same content".into())];

        assert!(similarity_renames(&deleted, &added).is_empty());
    }
}
