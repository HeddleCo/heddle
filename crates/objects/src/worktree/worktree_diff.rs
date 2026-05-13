// SPDX-License-Identifier: Apache-2.0
//! Diff utilities for blobs.

use crate::object::Blob;

/// Compute a simple diff between two blobs.
pub fn diff_blobs(old: &Blob, new: &Blob) -> Vec<DiffLine> {
    let Some(old_text) = old.content_str() else {
        return Vec::new();
    };
    let Some(new_text) = new.content_str() else {
        return Vec::new();
    };

    let diff = similar::TextDiff::configure()
        .algorithm(similar::Algorithm::Histogram)
        .diff_lines(old_text, new_text)
        .iter_all_changes()
        .map(|change| {
            let content = change.value().trim_end_matches('\n').to_string();
            match change.tag() {
                similar::ChangeTag::Delete => DiffLine::Removed(content),
                similar::ChangeTag::Insert => DiffLine::Added(content),
                similar::ChangeTag::Equal => DiffLine::Context(content),
            }
        })
        .collect();
    keep_annotations_with_inserted_items(diff)
}

fn keep_annotations_with_inserted_items(lines: Vec<DiffLine>) -> Vec<DiffLine> {
    let mut output = Vec::with_capacity(lines.len());
    let mut index = 0;

    while index < lines.len() {
        let Some(DiffLine::Context(annotation)) = lines.get(index) else {
            output.push(lines[index].clone());
            index += 1;
            continue;
        };

        if !is_decoration_line(annotation) {
            output.push(lines[index].clone());
            index += 1;
            continue;
        }

        let added_start = index + 1;
        let mut added_end = added_start;
        while matches!(lines.get(added_end), Some(DiffLine::Added(_))) {
            added_end += 1;
        }

        let has_inserted_item = first_meaningful_added_line(&lines[added_start..added_end])
            .is_some_and(is_item_declaration_line);
        let decorates_next_context = matches!(
            lines.get(added_end),
            Some(DiffLine::Context(next)) if is_item_declaration_line(next)
        );

        if added_end > added_start && has_inserted_item && decorates_next_context {
            output.push(DiffLine::Added(annotation.clone()));
            output.extend(lines[added_start..added_end].iter().cloned());
            output.push(lines[index].clone());
            index = added_end;
            continue;
        }

        output.push(lines[index].clone());
        index += 1;
    }

    output
}

fn first_meaningful_added_line(lines: &[DiffLine]) -> Option<&str> {
    lines.iter().find_map(|line| match line {
        DiffLine::Added(content) if !content.trim().is_empty() => Some(content.as_str()),
        _ => None,
    })
}

fn is_attribute_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("#[") || trimmed.starts_with("#![")
}

fn is_decoration_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    is_attribute_line(line)
        || trimmed.starts_with('@')
        || trimmed.starts_with("///")
        || trimmed.starts_with("//!")
        || trimmed.starts_with("/**")
        || trimmed.starts_with('*')
        || trimmed.starts_with("\"\"\"")
        || trimmed.starts_with("'''")
}

fn is_item_declaration_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    matches!(
        trimmed.split_whitespace().next(),
        Some(
            "fn" | "pub"
                | "async"
                | "const"
                | "struct"
                | "enum"
                | "trait"
                | "impl"
                | "mod"
                | "type"
                | "def"
                | "class"
                | "function"
                | "export"
                | "let"
                | "var"
        )
    )
}

/// A line in a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    /// Line present in both versions.
    Context(String),
    /// Line added in new version.
    Added(String),
    /// Line removed from old version.
    Removed(String),
}

impl DiffLine {
    /// Get the prefix for display.
    pub fn prefix(&self) -> &'static str {
        match self {
            DiffLine::Context(_) => " ",
            DiffLine::Added(_) => "+",
            DiffLine::Removed(_) => "-",
        }
    }

    /// Get the line content.
    pub fn content(&self) -> &str {
        match self {
            DiffLine::Context(s) | DiffLine::Added(s) | DiffLine::Removed(s) => s,
        }
    }
}