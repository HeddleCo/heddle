// SPDX-License-Identifier: Apache-2.0
//! Fail-closed ci-runner trust-set resolution for CI verdict verification.

use objects::object::{KeyBindingRegistry, KeyRole};

use crate::{AuthorshipVerification, Repository, TrustedKey};

/// One key the verifier may accept on a `heddle-ci-verdict-v1` signature.
///
/// Entries exist only for bindings that already passed
/// [`Repository::verify_known_actor_key`] with [`KeyRole::CiRunner`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiRunnerTrustEntry {
    pub algorithm: String,
    pub public_key: String,
    pub identity_ref: String,
}

/// Keys trusted to sign CI verdicts for a repository at resolution time.
///
/// An empty set is the fail-closed outcome: no runner key is trusted.
/// Callers must treat a missing membership check as reject, never as
/// "unsigned is fine."
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CiRunnerTrustSet {
    entries: Vec<CiRunnerTrustEntry>,
}

impl CiRunnerTrustSet {
    /// No trusted ci-runner keys. Verdict signatures fail closed.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn entries(&self) -> &[CiRunnerTrustEntry] {
        &self.entries
    }

    /// Whether `(algorithm, public_key)` is in this set.
    ///
    /// Comparison is case-insensitive to match
    /// [`Repository::verify_known_actor_key`] key identity.
    #[must_use]
    pub fn contains(&self, algorithm: &str, public_key: &str) -> bool {
        self.entries.iter().any(|entry| {
            entry.algorithm.eq_ignore_ascii_case(algorithm)
                && entry.public_key.eq_ignore_ascii_case(public_key)
        })
    }
}

impl Repository {
    /// Collect currently valid `ci-runner` bindings into a verdict trust set.
    ///
    /// A binding is included only when [`Self::verify_known_actor_key`]
    /// returns [`AuthorshipVerification::Verified`] for [`KeyRole::CiRunner`].
    /// That is the same authorization, revocation, and validity-window
    /// decision used for other KeyBinding roles:
    ///
    /// - an unauthorized registry yields an empty set
    /// - a recorded `revoked_at` excludes the binding
    /// - `valid_from` after the verifier clock excludes the binding
    /// - a non-`ci-runner` role cannot leak into the set
    ///
    /// Signer-asserted verdict timestamps are not consulted. A revoked
    /// binding stays out of the set even if a verdict claims to predate
    /// revocation. The verifier clock only rejects a future-dated
    /// authority-issued `valid_from`.
    #[must_use]
    pub fn resolve_ci_runner_trust_set(
        &self,
        registry: &KeyBindingRegistry,
        trusted_authority: &TrustedKey,
    ) -> CiRunnerTrustSet {
        let entries = registry
            .bindings
            .iter()
            .filter_map(|binding| {
                match self.verify_known_actor_key(
                    &binding.algorithm,
                    &binding.public_key,
                    KeyRole::CiRunner,
                    registry,
                    trusted_authority,
                ) {
                    AuthorshipVerification::Verified(identity_ref) => Some(CiRunnerTrustEntry {
                        algorithm: binding.algorithm.clone(),
                        public_key: binding.public_key.clone(),
                        identity_ref,
                    }),
                    _ => None,
                }
            })
            .collect();
        CiRunnerTrustSet { entries }
    }
}

#[cfg(test)]
#[path = "ci_runner_trust_tests.rs"]
mod tests;
