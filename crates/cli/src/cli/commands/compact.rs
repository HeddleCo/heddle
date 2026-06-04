// SPDX-License-Identifier: Apache-2.0
//! Compact JSON projection for `--output json-compact` (heddle#470).
//!
//! `--output json` is the full machine contract. An agent driving
//! Heddle only acts on a handful of those fields — `output_kind`,
//! `status`/`coordination_status`, `blockers`, `next_action`,
//! `changed_paths`/`changed_path_count`, `conflicts`/`conflict_count`.
//! Everything else is metadata it has to re-parse and discard.
//!
//! [`CompactOutput`] is the single shared shape that projection emits.
//! [`CompactProjection`] is implemented by every full command output
//! that has a decision surface; the operator family (`merge`, `ready`,
//! `continue`, `abort`, `sync`, `land`) derives its core fields from the
//! one shared [`OperatorCommandOutput`] projection so the compact shape
//! stays in lockstep as the full envelope grows, rather than each verb
//! hand-rolling its own subset.

use serde::Serialize;

use super::command_catalog::ActionTemplate;
use super::thread::CoordinationStatus;

/// The decision-surface projection emitted by `--output json-compact`.
///
/// Fields absent from a given command stay `None`/empty and are skipped
/// on the wire, so a command only emits the axes it actually has.
/// `output_kind` is always present so callers can still dispatch on the
/// discriminator exactly as they do for the full contract.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CompactOutput {
    pub output_kind: String,
    /// Operator-command lifecycle status (`landed`, `blocked`, `noop`,
    /// …). Mutually exclusive in practice with `coordination_status`,
    /// which is `status`'s analogue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordination_status: Option<CoordinationStatus>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    /// Always serialized (even `null`) so callers have a stable field to
    /// read the recommended next command from.
    pub next_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action_template: Option<ActionTemplate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_path_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_count: Option<usize>,
}

impl CompactOutput {
    pub(crate) fn new(output_kind: impl Into<String>) -> Self {
        Self {
            output_kind: output_kind.into(),
            status: None,
            coordination_status: None,
            blockers: Vec::new(),
            next_action: None,
            next_action_template: None,
            changed_paths: None,
            changed_path_count: None,
            conflicts: None,
            conflict_count: None,
        }
    }
}

/// A full command output that can project down to the compact decision
/// surface. Routing every compact-capable emit site through this trait
/// (via [`super::next_action::write_command_json`]) is the chokepoint
/// that keeps a new operator verb from silently shipping the full
/// envelope under `--output json-compact`.
pub(crate) trait CompactProjection {
    fn compact(&self) -> CompactOutput;
}
