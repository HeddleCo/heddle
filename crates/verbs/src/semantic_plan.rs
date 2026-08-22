// SPDX-License-Identifier: Apache-2.0
//! Pure semantic hot-event kind tokens and labels (no store I/O, no CLI deps).
//!
//! Owns the stable snake_case labels used by `heddle semantic hot` human/JSON
//! output and a pure mirror enum so callers can map clap / `semantic` kinds
//! without pulling CLI types into core.
//!
//! [`HotEventKind`](semantic::analysis::HotEventKind) lives in the optional
//! `semantic` crate. Bridge helpers under `cfg(feature = "semantic")` convert
//! tokens ↔ that enum; label helpers stay always-on string/token pure code.

/// Pure mirror of hot-event kinds used for planning and labels.
///
/// Variant names match `semantic::HotEventKind` and the CLI `HotEventKindArg`
/// clap surface one-to-one.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum HotEventKindToken {
    FileAdded,
    FileDeleted,
    FileModified,
    FileRenamed,
    FunctionExtracted,
    FunctionDeleted,
    FunctionRenamed,
    FunctionModified,
    FunctionMoved,
    SignatureChanged,
    DependencyChanged,
}

/// All tokens in a stable order (matches historical CLI match arm order).
pub const HOT_EVENT_KIND_TOKENS: &[HotEventKindToken] = &[
    HotEventKindToken::FileAdded,
    HotEventKindToken::FileDeleted,
    HotEventKindToken::FileModified,
    HotEventKindToken::FileRenamed,
    HotEventKindToken::FunctionExtracted,
    HotEventKindToken::FunctionDeleted,
    HotEventKindToken::FunctionRenamed,
    HotEventKindToken::FunctionModified,
    HotEventKindToken::FunctionMoved,
    HotEventKindToken::SignatureChanged,
    HotEventKindToken::DependencyChanged,
];

/// Human/JSON snake_case label for a hot-event kind token.
///
/// Mirrors historical CLI `human_event_kind` output (`file_added`, …).
pub fn hot_event_kind_label(kind: HotEventKindToken) -> &'static str {
    match kind {
        HotEventKindToken::FileAdded => "file_added",
        HotEventKindToken::FileDeleted => "file_deleted",
        HotEventKindToken::FileModified => "file_modified",
        HotEventKindToken::FileRenamed => "file_renamed",
        HotEventKindToken::FunctionExtracted => "function_extracted",
        HotEventKindToken::FunctionDeleted => "function_deleted",
        HotEventKindToken::FunctionRenamed => "function_renamed",
        HotEventKindToken::FunctionModified => "function_modified",
        HotEventKindToken::FunctionMoved => "function_moved",
        HotEventKindToken::SignatureChanged => "signature_changed",
        HotEventKindToken::DependencyChanged => "dependency_changed",
    }
}

/// Parse a hot-event kind from a string token.
///
/// Accepts snake_case labels (`file_added`), clap-style kebab-case
/// (`file-added`), and PascalCase variant names (`FileAdded`).
pub fn parse_hot_event_kind_token(raw: &str) -> Option<HotEventKindToken> {
    match raw {
        "file_added" | "file-added" | "FileAdded" => Some(HotEventKindToken::FileAdded),
        "file_deleted" | "file-deleted" | "FileDeleted" => Some(HotEventKindToken::FileDeleted),
        "file_modified" | "file-modified" | "FileModified" => Some(HotEventKindToken::FileModified),
        "file_renamed" | "file-renamed" | "FileRenamed" => Some(HotEventKindToken::FileRenamed),
        "function_extracted" | "function-extracted" | "FunctionExtracted" => {
            Some(HotEventKindToken::FunctionExtracted)
        }
        "function_deleted" | "function-deleted" | "FunctionDeleted" => {
            Some(HotEventKindToken::FunctionDeleted)
        }
        "function_renamed" | "function-renamed" | "FunctionRenamed" => {
            Some(HotEventKindToken::FunctionRenamed)
        }
        "function_modified" | "function-modified" | "FunctionModified" => {
            Some(HotEventKindToken::FunctionModified)
        }
        "function_moved" | "function-moved" | "FunctionMoved" => {
            Some(HotEventKindToken::FunctionMoved)
        }
        "signature_changed" | "signature-changed" | "SignatureChanged" => {
            Some(HotEventKindToken::SignatureChanged)
        }
        "dependency_changed" | "dependency-changed" | "DependencyChanged" => {
            Some(HotEventKindToken::DependencyChanged)
        }
        _ => None,
    }
}

/// Alias matching historical CLI name for the label helper.
#[inline]
pub fn human_event_kind(kind: HotEventKindToken) -> &'static str {
    hot_event_kind_label(kind)
}

// ---------------------------------------------------------------------------
// Optional bridge to `semantic::HotEventKind` (feature = "semantic")
// ---------------------------------------------------------------------------

#[cfg(feature = "semantic")]
use semantic::analysis::HotEventKind;

/// Map a pure token to [`HotEventKind`] (feature `semantic`).
#[cfg(feature = "semantic")]
pub fn map_hot_event_kind(token: HotEventKindToken) -> HotEventKind {
    match token {
        HotEventKindToken::FileAdded => HotEventKind::FileAdded,
        HotEventKindToken::FileDeleted => HotEventKind::FileDeleted,
        HotEventKindToken::FileModified => HotEventKind::FileModified,
        HotEventKindToken::FileRenamed => HotEventKind::FileRenamed,
        HotEventKindToken::FunctionExtracted => HotEventKind::FunctionExtracted,
        HotEventKindToken::FunctionDeleted => HotEventKind::FunctionDeleted,
        HotEventKindToken::FunctionRenamed => HotEventKind::FunctionRenamed,
        HotEventKindToken::FunctionModified => HotEventKind::FunctionModified,
        HotEventKindToken::FunctionMoved => HotEventKind::FunctionMoved,
        HotEventKindToken::SignatureChanged => HotEventKind::SignatureChanged,
        HotEventKindToken::DependencyChanged => HotEventKind::DependencyChanged,
    }
}

/// Map [`HotEventKind`] back to a pure token (feature `semantic`).
#[cfg(feature = "semantic")]
pub fn hot_event_kind_token(kind: HotEventKind) -> HotEventKindToken {
    match kind {
        HotEventKind::FileAdded => HotEventKindToken::FileAdded,
        HotEventKind::FileDeleted => HotEventKindToken::FileDeleted,
        HotEventKind::FileModified => HotEventKindToken::FileModified,
        HotEventKind::FileRenamed => HotEventKindToken::FileRenamed,
        HotEventKind::FunctionExtracted => HotEventKindToken::FunctionExtracted,
        HotEventKind::FunctionDeleted => HotEventKindToken::FunctionDeleted,
        HotEventKind::FunctionRenamed => HotEventKindToken::FunctionRenamed,
        HotEventKind::FunctionModified => HotEventKindToken::FunctionModified,
        HotEventKind::FunctionMoved => HotEventKindToken::FunctionMoved,
        HotEventKind::SignatureChanged => HotEventKindToken::SignatureChanged,
        HotEventKind::DependencyChanged => HotEventKindToken::DependencyChanged,
    }
}

/// Label a [`HotEventKind`] via the pure token table (feature `semantic`).
#[cfg(feature = "semantic")]
pub fn human_hot_event_kind(kind: HotEventKind) -> &'static str {
    hot_event_kind_label(hot_event_kind_token(kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_snake_case_and_stable() {
        assert_eq!(
            hot_event_kind_label(HotEventKindToken::FileAdded),
            "file_added"
        );
        assert_eq!(
            hot_event_kind_label(HotEventKindToken::SignatureChanged),
            "signature_changed"
        );
        assert_eq!(
            human_event_kind(HotEventKindToken::DependencyChanged),
            "dependency_changed"
        );
        assert_eq!(HOT_EVENT_KIND_TOKENS.len(), 11);
        for token in HOT_EVENT_KIND_TOKENS {
            let label = hot_event_kind_label(*token);
            assert!(label.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
            assert_eq!(parse_hot_event_kind_token(label), Some(*token));
        }
    }

    #[test]
    fn parse_accepts_snake_kebab_and_pascal() {
        assert_eq!(
            parse_hot_event_kind_token("file-modified"),
            Some(HotEventKindToken::FileModified)
        );
        assert_eq!(
            parse_hot_event_kind_token("FunctionMoved"),
            Some(HotEventKindToken::FunctionMoved)
        );
        assert_eq!(parse_hot_event_kind_token("not-a-kind"), None);
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn semantic_bridge_round_trips() {
        for token in HOT_EVENT_KIND_TOKENS {
            let kind = map_hot_event_kind(*token);
            assert_eq!(hot_event_kind_token(kind), *token);
            assert_eq!(human_hot_event_kind(kind), hot_event_kind_label(*token));
        }
    }
}
