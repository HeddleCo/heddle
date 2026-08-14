// SPDX-License-Identifier: Apache-2.0
//! Canonical content model for a CI verdict.
//!
//! Canonical bytes are `serde_json` over these structs in declaration order.
//! Maps are [`BTreeMap`]s, absent optional fields are omitted, and every schema
//! change that moves the bytes must bump [`CI_VERDICT_BODY_SCHEMA_VERSION`].

use std::collections::BTreeMap;

use objects::object::ContentHash;
use serde::{Deserialize, Serialize};

use super::body_details::{Execution, LogRef, Outcome, Repro};

/// Current canonical [`CiVerdictBody`] schema version.
pub const CI_VERDICT_BODY_SCHEMA_VERSION: u32 = 1;

/// The complete conclusion-bearing content of a CI verdict.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CiVerdictBody {
    /// Canonical body schema version.
    pub schema_version: u32,
    /// Repository this verdict describes.
    pub repo: String,
    /// Source state that was evaluated.
    pub state: StateRef,
    /// Branch or speculative-merge basis actually evaluated.
    pub basis: Basis,
    /// Check identity and resolved parameters.
    pub check: CheckDescriptor,
    /// Terminal conclusion and optional failure detail.
    pub outcome: Outcome,
    /// Runner and timing metadata.
    pub execution: Execution,
    /// Finalized log reference; log bytes are never inlined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<LogRef>,
    /// Exact local reproduction recipe.
    pub repro: Repro,
    /// Canonical CheckSet digest used with `check.node_id` for authoritative gates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_set_digest: Option<String>,
}

impl CiVerdictBody {
    /// Deterministic bytes hashed into [`Self::content_hash`].
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("CiVerdictBody is always serializable")
    }

    /// BLAKE3 [`ContentHash`] of the canonical body bytes.
    #[must_use]
    pub fn content_hash(&self) -> ContentHash {
        ContentHash::compute(&self.canonical_bytes())
    }
}

/// Reference to the immutable source state described by the body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StateRef {
    /// Transfer-stable source-state content digest.
    pub content_hash: String,
    /// Physical source-state identifier.
    pub change_id: String,
    /// Optional rewrite-stable lineage identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_change_id: Option<String>,
}

/// Exact tree evaluated and how it relates to a merge target.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Basis {
    /// Branch-versus-merge discriminator.
    pub kind: BasisKind,
    /// Digest of the exact evaluated tree, including speculative merges.
    pub evaluated_tree_digest: String,
}

/// Branch-versus-speculative-merge discriminator.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BasisKind {
    /// The branch tree was evaluated as pushed.
    #[default]
    Branch,
    /// The branch was evaluated after merging it with a target.
    MergedWith {
        /// Target state used for the speculative merge.
        target_state: String,
        /// Number of commits the branch was behind that target.
        behind_count: u32,
        /// Merge implementation version used to materialize the tree.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        merge_algorithm_version: Option<String>,
        /// Conflict policy applied while materializing the tree.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conflict_policy: Option<String>,
    },
}

/// Check identity, command, and resolved inputs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CheckDescriptor {
    /// Unique check name within the repository definition.
    pub name: String,
    /// Whether the check gates, advises, or only informs.
    pub class: CheckClass,
    /// Digest of the authored check definition.
    pub definition_digest: String,
    /// Exact argument vector executed by the check.
    pub command: Vec<String>,
    /// Optional immutable container image digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_digest: Option<String>,
    /// Optional toolchain identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    /// Sorted resolved parameters, preventing cheap-check substitution.
    pub params: BTreeMap<String, String>,
    /// Service containers required by the check.
    pub services: Vec<String>,
    /// Check node within the body-level CheckSet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

/// Whether a check gates a merge or only contributes advisory context.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckClass {
    /// A non-green verdict blocks the merge when signer policy also allows it.
    Required,
    /// Reported but never gates.
    #[default]
    Advisory,
    /// Context-only check, such as a metric.
    Informational,
}
