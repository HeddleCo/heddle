// SPDX-License-Identifier: Apache-2.0
//! # heddle#327 spike PoC — one verb, both ways
//!
//! THROWAWAY crate. Deleted by heddle#205 when it lands `crates/cli-macro/`.
//!
//! This crate reproduces ONE real CLI verb — `heddle init` — in isolation and
//! emits its `--output json` JSON Schema **two ways**, so the spike can measure
//! the schemars-vs-custom-emitter tradeoff against a real shape:
//!
//! * [`schemars_path`] — derive `schemars::JsonSchema` on the output struct and
//!   call `schema_for!`. This is the path heddle uses *today* (the hand-written
//!   mirror structs in `crates/cli/src/cli/commands/schemas.rs` derive
//!   `JsonSchema`). The spike question is whether a macro can emit this derive
//!   from the SAME declaration that drives clap, deleting the mirror.
//! * [`custom_path`] — a hand-written emitter that walks an explicit field
//!   table and builds the schema `serde_json::Value` directly, with no derive.
//!   This is the "tighter, net-new code" alternative #205 names.
//!
//! The faithful reproduction target is the registered `InitSchema` mirror at
//! `crates/cli/src/cli/commands/schemas.rs` (`pub struct InitSchema`) and the
//! documented sample at `docs/json-schemas.md` (`## heddle init --output json`).
//! Field set + doc-comment text are copied verbatim from those sources so the
//! measurement reflects the production shape, not a toy.
//!
//! Run `cargo test -p heddle-cli-macro-poc -- --nocapture` to print both
//! schemas + the measurement table the spike doc cites.

use clap::Parser;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// INPUT SHAPE (clap) — copied from the real `InitArgs`
// (`crates/cli/src/cli/cli_args/commands_args.rs:12`).
//
// The spike question for the input side is NOT "can a macro emit a clap
// struct" (clap's own derive already does that) but "can ONE declaration emit
// BOTH this clap struct AND the output schema below". This struct is the input
// half of that single declaration; `InitOutput` is the output half. See the
// spec (`docs/spikes/heddle-327-cli-macro-shape.md`) for the unifying
// `#[heddle_verb]` attribute that ties the two together.
// ---------------------------------------------------------------------------

/// Initialize Heddle metadata in a repository.
#[derive(Debug, Parser)]
pub struct InitArgs {
    /// Directory to initialize (default: current directory).
    pub path: Option<std::path::PathBuf>,

    /// Principal name for attribution.
    #[arg(long)]
    pub principal_name: Option<String>,

    /// Principal email for attribution.
    #[arg(long)]
    pub principal_email: Option<String>,

    /// Install harness integrations after init.
    #[arg(long)]
    pub install_harnesses: Option<String>,

    /// Skip harness integration install prompts.
    #[arg(long)]
    pub no_harness_install: bool,

    /// Preferred install scope (`repo` or `user`).
    #[arg(long, visible_alias = "scope", default_value = "repo")]
    pub harness_install_scope: String,

    /// Overwrite Heddle-managed integration entries when needed.
    #[arg(long)]
    pub harness_install_force: bool,
}

// ---------------------------------------------------------------------------
// OUTPUT SHAPE — field set copied from the registered `InitSchema` mirror
// (`crates/cli/src/cli/commands/schemas.rs`, `pub struct InitSchema`).
//
// The doc comment on each field IS the attribute-design demonstration: schemars
// lifts `///` doc comments into the schema's `description` natively (proven by
// `schemars_emits_field_descriptions_from_doc_comments`). That answers spike
// question #1 — narrative descriptions live in `#[doc]`, no sibling attribute
// needed for them. Examples are the part schemars handles awkwardly; see
// `init_example` + the spec's "examples" section.
// ---------------------------------------------------------------------------

/// `heddle init --output json` response.
///
/// Initialize Heddle metadata. In a plain Git repository this creates the
/// `.heddle` sidecar and updates the local `.git/info/exclude` file for Heddle
/// metadata only; it does not import Git history or write Git-tracked files.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(example = "init_example")]
pub struct InitOutput {
    /// Stable output discriminator. Always `"init"`.
    pub output_kind: String,
    /// Always `initialized` on success.
    pub status: String,
    /// Always `init` on success.
    pub action: String,
    /// Path to the initialized `.heddle` metadata directory.
    pub path: String,
    /// Repository capability after init, e.g. `git-overlay`.
    pub repository_mode: String,
    /// Whether init detected an existing Git repo.
    pub git_detected: bool,
    /// Whether Heddle metadata is now present.
    pub heddle_initialized: bool,
    /// Side effect outside `.heddle`. Currently always false.
    pub installed_heddleignore: bool,
    /// Whether a signing principal was configured during init.
    pub principal_configured: bool,
    /// Principal configuration status string.
    pub principal_status: String,
    /// Where the principal identity was sourced from, if any.
    pub principal_source: Option<String>,
    /// Resolved principal identity, if one was configured.
    pub principal: Option<InitPrincipal>,
    /// Suggested follow-up to configure a principal, if unset.
    pub principal_recommended_action: Option<String>,
    /// Human-readable, machine-preserved list of what init changed.
    pub side_effects: Vec<String>,
    /// Human summary line.
    pub message: String,
    /// Primary verification-guided next command.
    pub next_action: Option<String>,
    /// Alias of `next_action` for the shared recovery-advice contract.
    pub recommended_action: Option<String>,
}

/// Resolved principal identity attached to an init reply.
#[derive(Debug, Serialize, JsonSchema)]
pub struct InitPrincipal {
    /// Principal display name.
    pub name: String,
    /// Principal email.
    pub email: String,
}

/// Canonical example payload — the same sample documented at
/// `docs/json-schemas.md` (`## heddle init --output json`). schemars consumes
/// this via `#[schemars(example = "init_example")]` and folds it into the
/// schema's `examples`.
pub fn init_example() -> InitOutput {
    InitOutput {
        output_kind: "init".into(),
        status: "initialized".into(),
        action: "init".into(),
        path: "/repo/.heddle".into(),
        repository_mode: "git-overlay".into(),
        git_detected: true,
        heddle_initialized: true,
        installed_heddleignore: false,
        principal_configured: false,
        principal_status: "unset".into(),
        principal_source: None,
        principal: None,
        principal_recommended_action: Some("heddle init --principal-name <name>".into()),
        side_effects: vec![
            "created Heddle sidecar for the existing Git repository".into(),
            "updated .git/info/exclude for Heddle metadata".into(),
            "left Git-tracked files untouched".into(),
        ],
        message: "Initialized Heddle data in /repo/.heddle for Git-overlay workflows".into(),
        next_action: Some("heddle adopt --ref main".into()),
        recommended_action: Some("heddle adopt --ref main".into()),
    }
}

/// PATH A — schemars derive. One line at the call site once the macro emits the
/// derive; this is what heddle does today via the hand-written mirror.
pub mod schemars_path {
    use super::*;

    /// JSON Schema for `init --output json` via `schemars::schema_for!`.
    pub fn schema() -> Value {
        let root = schemars::schema_for!(InitOutput);
        serde_json::to_value(&root).expect("schemars RootSchema serializes")
    }
}

/// PATH B — hand-written emitter. Represents the code a *custom* heddle emitter
/// macro would generate: an explicit field table → schema `Value`, no derive,
/// no `schemars` dependency. The point of writing it by hand here is to measure
/// what that control buys (and costs) versus the derive.
pub mod custom_path {
    use super::*;

    /// A single field's contribution to the object schema.
    struct Field {
        name: &'static str,
        /// JSON Schema fragment for the field's type.
        ty: Value,
        /// `///`-equivalent narrative.
        description: &'static str,
        /// Whether the field is required (non-`Option`).
        required: bool,
    }

    fn string_ty() -> Value {
        json!({ "type": "string" })
    }
    fn bool_ty() -> Value {
        json!({ "type": "boolean" })
    }
    fn nullable_string_ty() -> Value {
        json!({ "type": ["string", "null"] })
    }
    fn string_array_ty() -> Value {
        json!({ "type": "array", "items": { "type": "string" } })
    }
    fn principal_ty() -> Value {
        json!({
            "type": ["object", "null"],
            "properties": {
                "name": { "type": "string", "description": "Principal display name." },
                "email": { "type": "string", "description": "Principal email." }
            },
            "required": ["name", "email"],
            "additionalProperties": false
        })
    }

    /// The explicit field table. In production this is exactly what the macro
    /// would emit from the annotated struct — one row per declared field.
    fn fields() -> Vec<Field> {
        vec![
            Field {
                name: "output_kind",
                ty: json!({ "type": "string", "const": "init" }),
                description: "Stable output discriminator. Always \"init\".",
                required: true,
            },
            Field {
                name: "status",
                ty: string_ty(),
                description: "Always `initialized` on success.",
                required: true,
            },
            Field {
                name: "action",
                ty: string_ty(),
                description: "Always `init` on success.",
                required: true,
            },
            Field {
                name: "path",
                ty: string_ty(),
                description: "Path to the initialized `.heddle` metadata directory.",
                required: true,
            },
            Field {
                name: "repository_mode",
                ty: string_ty(),
                description: "Repository capability after init, e.g. `git-overlay`.",
                required: true,
            },
            Field {
                name: "git_detected",
                ty: bool_ty(),
                description: "Whether init detected an existing Git repo.",
                required: true,
            },
            Field {
                name: "heddle_initialized",
                ty: bool_ty(),
                description: "Whether Heddle metadata is now present.",
                required: true,
            },
            Field {
                name: "installed_heddleignore",
                ty: bool_ty(),
                description: "Side effect outside `.heddle`. Currently always false.",
                required: true,
            },
            Field {
                name: "principal_configured",
                ty: bool_ty(),
                description: "Whether a signing principal was configured during init.",
                required: true,
            },
            Field {
                name: "principal_status",
                ty: string_ty(),
                description: "Principal configuration status string.",
                required: true,
            },
            Field {
                name: "principal_source",
                ty: nullable_string_ty(),
                description: "Where the principal identity was sourced from, if any.",
                required: false,
            },
            Field {
                name: "principal",
                ty: principal_ty(),
                description: "Resolved principal identity, if one was configured.",
                required: false,
            },
            Field {
                name: "principal_recommended_action",
                ty: nullable_string_ty(),
                description: "Suggested follow-up to configure a principal, if unset.",
                required: false,
            },
            Field {
                name: "side_effects",
                ty: string_array_ty(),
                description: "Human-readable, machine-preserved list of what init changed.",
                required: true,
            },
            Field {
                name: "message",
                ty: string_ty(),
                description: "Human summary line.",
                required: true,
            },
            Field {
                name: "next_action",
                ty: nullable_string_ty(),
                description: "Primary verification-guided next command.",
                required: false,
            },
            Field {
                name: "recommended_action",
                ty: nullable_string_ty(),
                description: "Alias of `next_action` for the shared recovery-advice contract.",
                required: false,
            },
        ]
    }

    /// JSON Schema for `init --output json` built by the custom emitter.
    pub fn schema() -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for f in fields() {
            let mut ty = f.ty;
            if let Value::Object(ref mut map) = ty {
                map.insert("description".into(), Value::String(f.description.into()));
            }
            properties.insert(f.name.into(), ty);
            if f.required {
                required.push(Value::String(f.name.into()));
            }
        }
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "InitOutput",
            "description": "`heddle init --output json` response.",
            "type": "object",
            "properties": Value::Object(properties),
            "required": Value::Array(required),
            "additionalProperties": false,
            "examples": [serde_json::to_value(init_example()).expect("example serializes")]
        })
    }
}

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
