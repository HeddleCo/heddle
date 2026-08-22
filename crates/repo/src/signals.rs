// SPDX-License-Identifier: Apache-2.0
//! Registration seam for capture-time risk signals.
//!
//! Mirrors the [`crate::lazy_hydrator`] precedent: `repo` owns only the
//! trait and the registration slot on [`Repository`]; the concrete
//! implementation lives in `state-review` (which owns the signal
//! modules), keeping this crate free of that dependency. Entry-point
//! binaries opt in once at startup with
//! [`Repository::set_signal_computer`]; unregistered repos skip signal
//! computation entirely, which is identical to the historical
//! "feature off" on-disk shape.

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
