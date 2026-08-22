// SPDX-License-Identifier: Apache-2.0
//! Per-repo configuration for the risk-signal modules.
//!
//! Lives under `[review.signals]` in `.heddle/config.toml`. Each module has
//! its own sub-table; defaults are conservative so a fresh repo isn't noisy.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewSignalsConfig {
    #[serde(default)]
    pub novelty: NoveltyConfig,
    #[serde(default)]
    pub test_reachability: TestReachabilityConfig,
    #[serde(default)]
    pub pattern_deviation: PatternDeviationConfig,
    #[serde(default)]
    pub invariant_adjacency: InvariantAdjacencyConfig,
    #[serde(default)]
    pub self_flagged_uncertainty: SelfFlaggedUncertaintyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoveltyConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tolerance below which a shape is considered "appears elsewhere".
    /// Lower values fire more aggressively. Range 0..=1.
    #[serde(default = "default_novelty_tolerance")]
    pub tolerance: f32,
}

impl Default for NoveltyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tolerance: default_novelty_tolerance(),
        }
    }
}

fn default_novelty_tolerance() -> f32 {
    0.15
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReachabilityConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Skip the module when the repo has fewer than this many test
    /// functions — avoids noise on greenfield repos.
    #[serde(default = "default_min_tests")]
    pub min_test_functions_in_repo: u32,
}

impl Default for TestReachabilityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_test_functions_in_repo: default_min_tests(),
        }
    }
}

fn default_min_tests() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternDeviationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Divergence score above which the signal fires. Range 0..=1.
    #[serde(default = "default_pattern_threshold")]
    pub threshold: f32,
}

impl Default for PatternDeviationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: default_pattern_threshold(),
        }
    }
}

fn default_pattern_threshold() -> f32 {
    0.6
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantAdjacencyConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for InvariantAdjacencyConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfFlaggedUncertaintyConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Cap per state so an agent can't drown the signal set.
    #[serde(default = "default_self_flag_cap")]
    pub max_per_state: u32,
}

impl Default for SelfFlaggedUncertaintyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_per_state: default_self_flag_cap(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_self_flag_cap() -> u32 {
    5
}
