// SPDX-License-Identifier: Apache-2.0
//! # heddle#327 spike PoC — one verb, both ways
//!
//! THROWAWAY crate. Deleted by heddle#205 when it lands `crates/cli-macro/`.
//!
//! This crate reproduces ONE real CLI verb — `heddle init` — in isolation and
//! emits its `--output json` JSON Schema **two ways**, so the spike can measure
//! the schemars-vs-custom-emitter tradeoff against the REAL production shape:
//!
//! * [`schemars_path`] — derive `schemars::JsonSchema` on the output struct and
//!   call `schema_for!`. This is the path heddle uses *today* (the hand-written
//!   mirror struct `InitSchema` in `crates/cli/src/cli/commands/schemas.rs`
//!   derives `JsonSchema`). The spike question is whether a macro can emit this
//!   derive from the SAME declaration that drives clap, deleting the mirror.
//! * [`custom_path`] — a hand-written emitter that walks an explicit field
//!   table and builds the schema `serde_json::Value` directly, with no derive.
//!   This is the "tighter, net-new code" alternative #205 names.
//!
//! **This PoC mirrors the real `init.rs` arg/output types** so the measured
//! drift/discriminator numbers reflect the production surface, not a toy:
//! - [`args`] mirrors the real `InitArgs` (`clap::Args`, wired through a
//!   `Subcommand` enum) — not a standalone `clap::Parser`.
//! - [`output`] mirrors the real private `InitOutput` field-for-field, INCLUDING
//!   the `#[serde(skip_serializing)] #[serde(rename = "verification")]` `trust`
//!   field that the hand-written mirror drops. That field is why deriving on the
//!   real struct is NOT semantics-free: schemars re-introduces a `verification`
//!   schema property the wire bytes never emit.
//!
//! Run `cargo test -p heddle-cli-macro-poc -- --nocapture` to print both
//! schemas + the measurement table; the assertions in `tests/measure.rs` pin
//! the drift, the discriminator gap, and the typed-example/doc-sample divergence.

use serde_json::Value;

pub mod args;
pub mod custom_path;
pub mod output;
pub mod schemars_path;

pub use output::{InitOutput, InitPrincipalOutput, RepositoryVerificationState, init_example};

/// Top-level keys the documented sample at `docs/json-schemas.md`
/// (`## heddle init --output json`) asserts. The drift gate
/// (`heddle doctor schemas`) checks exactly these against the registered
/// schema's `properties` keys, so any emitter the macro chooses MUST expose at
/// least this set as properties.
pub fn documented_sample_keys() -> Vec<&'static str> {
    vec![
        "output_kind",
        "status",
        "action",
        "path",
        "repository_mode",
        "git_detected",
        "heddle_initialized",
        "installed_heddleignore",
        "principal_configured",
        "side_effects",
        "message",
        "next_action",
        "recommended_action",
    ]
}

/// Extract the `properties` keys from either path's schema, regardless of where
/// the path puts them (schemars nests under `properties`; so does the custom
/// emitter — but schemars may also use `$ref`/`definitions`, which this
/// surfaces if present).
pub fn property_keys(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}
