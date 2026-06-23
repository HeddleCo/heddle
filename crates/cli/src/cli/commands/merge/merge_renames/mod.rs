// SPDX-License-Identifier: Apache-2.0
//! Merge-specific rename adapter wiring for CLI-only semantic scoring.

// Rename-matcher tests require AST-based semantic similarity to score
// modified-renames above the threshold. Without `--features semantic`
// the shared merge crate still runs with delta/path scoring only. Run
// `cargo test -p cli --features semantic merge_renames` to exercise the
// semantic adapter.
#[cfg(all(test, feature = "semantic"))]
mod tests;

pub(super) use ::merge::rename::MergeRenameMap;

pub(super) fn rename_matcher_config() -> ::merge::rename::RenameMatcherConfig {
    let config = ::merge::rename::RenameMatcherConfig::default();
    #[cfg(feature = "semantic")]
    {
        config.with_semantic_scorer(compute_semantic_similarity)
    }
    #[cfg(not(feature = "semantic"))]
    {
        config
    }
}

#[cfg(feature = "semantic")]
fn compute_semantic_similarity(
    from_path: &str,
    to_path: &str,
    from_content: &[u8],
    to_content: &[u8],
) -> f64 {
    let Ok(from_str) = std::str::from_utf8(from_content) else {
        return 0.0;
    };
    let Ok(to_str) = std::str::from_utf8(to_content) else {
        return 0.0;
    };

    let language = semantic::parser::Language::from_path(std::path::Path::new(from_path));
    let language = if language == semantic::parser::Language::Unknown {
        semantic::parser::Language::from_path(std::path::Path::new(to_path))
    } else {
        language
    };

    semantic::analysis::analysis_similarity::compute_similarity_with_language(
        from_str,
        to_str,
        semantic::analysis::analysis_similarity::SimilarityMethod::Ast,
        language,
    )
}
