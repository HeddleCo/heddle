// SPDX-License-Identifier: Apache-2.0
//! Real `--output json` wire payloads for the CLI verbs.
//!
//! These are the serialization structs the commands emit, not schema
//! mirrors: `Serialize` and `JsonSchema` derive from the same definition,
//! so a skip-serialized field cannot reappear on the published schema.
//! `crates/cli` re-exports each payload at its historical path, and
//! [`crate::cli::commands::schemas`] registers these types directly
//! (InitOutput precedent).
//!
//! The `#[schemars(rename)]` attributes keep the published `$defs` titles
//! stable while the Rust types carry their natural names. Types whose
//! serialization is hand-written (`OperatorCommandOutput`, `ReadyOutput`)
//! pair their `Serialize` impl with a manual `JsonSchema` built from a
//! private shape struct in the same file, so the schema and the serializer
//! stay one maintenance unit.

pub mod core_loop;
pub mod operator;
pub mod ready;

pub use core_loop::{CommitOutput, SnapshotAgentOutput, SnapshotOutput, SnapshotPrincipalOutput, UndoRedoOutput};
pub use operator::{
    OperatorAction, OperatorCommandEnvelope, OperatorCommandOutput, VerificationClaimPolicy,
};
pub use ready::{ReadyChecksSummary, ReadyOutput, ReadyReadinessSummary, ready_blocked_by_missing_intent};
