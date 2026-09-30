// SPDX-License-Identifier: Apache-2.0
//! Pure staleness checks for annotation source hashes.

use std::path::Path;

use super::{Annotation, AnnotationScope, ContentHash};

/// Result of checking an annotation's freshness against current code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StalenessStatus {
    /// Source hash matches -- annotation is current.
    Fresh,
    /// Source at the annotated scope has changed since the annotation was written.
    SourceChanged {
        old_hash: ContentHash,
        new_hash: ContentHash,
    },
    /// The file referenced by the annotation no longer exists in the tree.
    FileMissing,
    /// Symbol referenced by annotation no longer exists in the file.
    SymbolMissing { symbol: String },
    /// More than one definition matches the annotation's symbol selector, so
    /// it no longer identifies one piece of source. The authored selector is
    /// kept; `candidate_lines` are the 1-indexed inclusive matches.
    SymbolAmbiguous {
        symbol: String,
        candidate_lines: Vec<(u32, u32)>,
    },
    /// More than one file passed the rename confidence threshold.
    AmbiguousFileMove { candidate_paths: Vec<String> },
    /// No provenance data stored -- staleness cannot be determined.
    Unknown,
}

/// How a symbol selector resolved against current source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolResolution {
    /// Exactly one definition matches; 1-indexed inclusive lines.
    Resolved { start: u32, end: u32 },
    /// No definition matches.
    Missing,
    /// More than one definition matches.
    Ambiguous { candidate_lines: Vec<(u32, u32)> },
}

/// Check an annotation's staleness against already-loaded source bytes.
pub fn annotation_status_for_source(
    annotation: &Annotation,
    scope: &AnnotationScope,
    source: &[u8],
    file_path: &Path,
) -> StalenessStatus {
    annotation_status_for_source_with_symbol_resolver(
        annotation,
        scope,
        source,
        file_path,
        resolve_current_symbol,
    )
}

/// Check an annotation's staleness with an injected symbol resolver.
pub fn annotation_status_for_source_with_symbol_resolver(
    annotation: &Annotation,
    scope: &AnnotationScope,
    source: &[u8],
    file_path: &Path,
    mut resolve_symbol: impl FnMut(&[u8], &Path, &str, Option<(u32, u32)>) -> SymbolResolution,
) -> StalenessStatus {
    let Some(revision) = annotation.current_revision() else {
        return StalenessStatus::Unknown;
    };
    let expected_hash = match &revision.source_hash {
        Some(h) => h,
        None => return StalenessStatus::Unknown,
    };

    let scoped_bytes = match scope {
        AnnotationScope::File => source.to_vec(),
        AnnotationScope::Lines(start, end) => extract_line_range(source, *start, *end),
        AnnotationScope::Symbol {
            name,
            resolved_lines,
        } => match resolve_symbol(source, file_path, name, *resolved_lines) {
            SymbolResolution::Resolved { start, end } => extract_line_range(source, start, end),
            SymbolResolution::Missing => {
                return StalenessStatus::SymbolMissing {
                    symbol: name.clone(),
                };
            }
            SymbolResolution::Ambiguous { candidate_lines } => {
                return StalenessStatus::SymbolAmbiguous {
                    symbol: name.clone(),
                    candidate_lines,
                };
            }
        },
    };

    let current_hash = ContentHash::compute(&scoped_bytes);
    if current_hash == *expected_hash {
        StalenessStatus::Fresh
    } else {
        StalenessStatus::SourceChanged {
            old_hash: *expected_hash,
            new_hash: current_hash,
        }
    }
}

/// Extract bytes for a line range from source content.
///
/// Lines are 1-indexed. Returns the bytes spanning `start..=end` lines
/// (inclusive on both ends), joined with newlines.
pub fn extract_line_range(source: &[u8], start: u32, end: u32) -> Vec<u8> {
    let text = std::str::from_utf8(source).unwrap_or("");
    let lines: Vec<&str> = text.lines().collect();
    let start_idx = (start as usize).saturating_sub(1);
    let end_idx = (end as usize).min(lines.len());
    if start_idx >= end_idx {
        return Vec::new();
    }
    lines[start_idx..end_idx].join("\n").into_bytes()
}

/// Resolve a symbol using the stored line range.
///
/// Repository builds with semantic support inject a tree-sitter resolver at the
/// I/O boundary; the no-store core keeps this fallback pure and dependency-free.
pub fn resolve_current_symbol(
    _source: &[u8],
    _file_path: &Path,
    _symbol: &str,
    stored: Option<(u32, u32)>,
) -> SymbolResolution {
    match stored {
        Some((start, end)) => SymbolResolution::Resolved { start, end },
        None => SymbolResolution::Missing,
    }
}
