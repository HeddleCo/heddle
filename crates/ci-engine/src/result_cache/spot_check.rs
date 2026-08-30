// SPDX-License-Identifier: Apache-2.0
//! Deterministic sampling of cache hits for fail-closed re-execution.

use super::key::{CacheKey, entry_id_bytes};

/// Policy for re-running a sampled fraction of cache hits.
///
/// A disagreement between the cached entry and the fresh run is a hard
/// error ([`super::ResultCacheError::SpotCheckDivergence`]) — never a
/// silent fallback to either result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotCheck {
    /// Never re-run a cache hit. Tests use this to prove reuse.
    Never,
    /// Re-run every cache hit. Tests use this to prove fail-closed.
    Always,
    /// Re-run `numerator / denominator` of hits, chosen from the entry id.
    Fraction {
        /// Hits sampled per `denominator` keys.
        numerator: u32,
        /// Sampling modulus. Zero is treated as never.
        denominator: u32,
    },
}

impl Default for SpotCheck {
    fn default() -> Self {
        Self::Fraction {
            numerator: 1,
            denominator: 32,
        }
    }
}

impl SpotCheck {
    /// Whether this policy samples `key` / `check_name`.
    #[must_use]
    pub fn should_sample(&self, key: &CacheKey, check_name: &str) -> bool {
        match *self {
            Self::Never => false,
            Self::Always => true,
            Self::Fraction {
                numerator,
                denominator,
            } => {
                if denominator == 0 || numerator == 0 {
                    return false;
                }
                if numerator >= denominator {
                    return true;
                }
                let bytes = entry_id_bytes(key, check_name);
                let bucket = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                bucket % denominator < numerator
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crypto::{Basis, BasisKind, StateRef};

    use super::*;
    use crate::model::ExecutionContext;

    fn key() -> CacheKey {
        let mut check = ci_config::Check::new("build", vec!["true".to_string()]);
        check.timeout_secs = 1;
        check.supersede = false;
        CacheKey::derive(
            &std::collections::BTreeMap::new(),
            &ExecutionContext {
                repo: "test/repo".to_string(),
                state: StateRef {
                    content_hash: "state".to_string(),
                    change_id: "change".to_string(),
                    logical_change_id: None,
                },
                basis: Basis {
                    kind: BasisKind::Branch,
                    evaluated_tree_digest: "tree".to_string(),
                },
                definition_digest: "definition".to_string(),
                toolchain: None,
                pick_id: None,
                attempt: 1,
                runner: None,
                image_digest: None,
            },
            &check,
        )
    }

    #[test]
    fn fraction_is_deterministic_for_a_key() {
        let key = key();
        let policy = SpotCheck::Fraction {
            numerator: 1,
            denominator: 2,
        };
        let first = policy.should_sample(&key, "build");
        assert_eq!(first, policy.should_sample(&key, "build"));
        assert!(SpotCheck::Always.should_sample(&key, "build"));
        assert!(!SpotCheck::Never.should_sample(&key, "build"));
    }
}
