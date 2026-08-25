// SPDX-License-Identifier: Apache-2.0
//! Registration seam for capture-time risk signals.
//!
//! Mirrors the [`crate::lazy_hydrator`] precedent: `repo` owns only the
//! trait and the registration slot on [`Repository`]; the concrete
//! implementation lives in `state-review` (which owns the signal
//! modules), keeping this crate free of that dependency. Entry-point
//! binaries opt in once at startup with [`install_default_computer`];
//! unregistered repos skip signal computation entirely, which is identical to
//! the historical "feature off" on-disk shape. Embedders can still override
//! the process default per repository with [`Repository::set_signal_computer`].

use std::collections::HashMap;

use objects::{
    error::Result,
    object::{ContentHash, State, Tree},
};

use crate::Repository;

pub trait SignalComputer: Send + Sync {
    /// Run the signal registry against a freshly-built `(prior, new)`
    /// pair, encode any fired signals as a `RiskSignalBlob`, persist it,
    /// and return its hash for attachment to `new`.
    ///
    /// `Ok(None)` covers "no signals fired" and any internal failure the
    /// computer chose to swallow — capture must never fail because of a
    /// signal hiccup.
    fn compute_and_persist(
        &self,
        repo: &Repository,
        prior: Option<&State>,
        new: &State,
        new_index: Option<&ContentHash>,
        source_blobs: Option<&HashMap<ContentHash, &[u8]>>,
        source_trees: Option<&HashMap<ContentHash, &Tree>>,
    ) -> Result<Option<ContentHash>>;
}

/// Process-wide fallback computer. Entry-point binaries install the
/// concrete implementation once (instead of per-repository), so every
/// snapshot path — capture, commit, revert, undo, expand — computes
/// signals exactly like the pre-registration base behavior.
static GLOBAL_DEFAULT: std::sync::RwLock<Option<std::sync::Arc<dyn SignalComputer>>> =
    std::sync::RwLock::new(None);

pub fn install_default_computer(computer: std::sync::Arc<dyn SignalComputer>) {
    *GLOBAL_DEFAULT
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(computer);
}

#[cfg(feature = "tree-sitter-symbols")]
pub(crate) fn effective_computer(
    instance: Option<std::sync::Arc<dyn SignalComputer>>,
) -> Option<std::sync::Arc<dyn SignalComputer>> {
    instance.or_else(|| {
        GLOBAL_DEFAULT
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    })
}
