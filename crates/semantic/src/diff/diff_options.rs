// SPDX-License-Identifier: Apache-2.0
//! Semantic diff options.

use super::diff_types::SemanticBudget;
use crate::analysis::SimilarityMethod;

/// Options for semantic diff analysis.
#[derive(Clone, Debug)]
pub struct SemanticDiffOptions {
    /// Similarity threshold for detecting renames (0.0 to 1.0).
    pub rename_threshold: f64,
    /// Method for computing similarity.
    pub similarity_method: SimilarityMethod,
    /// Whether to analyze function-level changes.
    pub analyze_functions: bool,
    /// Whether to detect import/dependency changes.
    pub analyze_dependencies: bool,
    /// Resource limits for semantic analysis.
    pub budget: SemanticBudget,
}

impl Default for SemanticDiffOptions {
    fn default() -> Self {
        Self {
            rename_threshold: 0.6,
            similarity_method: SimilarityMethod::Lines,
            analyze_functions: true,
            analyze_dependencies: true,
            budget: SemanticBudget::default(),
        }
    }
}