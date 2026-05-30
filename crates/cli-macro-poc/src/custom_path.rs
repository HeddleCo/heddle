// SPDX-License-Identifier: Apache-2.0
//! PATH B — hand-written emitter. Represents the code a *custom* heddle emitter
//! macro would generate: an explicit field table → schema `Value`, no derive,
//! no `schemars` dependency. The point of writing it by hand here is to measure
//! what that control buys (and costs) versus the derive.
//!
//! A custom emitter walks the SERIALIZED field set, so it naturally omits
//! `#[serde(skip_serializing)]` fields like `trust`/`verification` — the schema
//! it produces matches the wire bytes by construction, with no phantom
//! `verification` property. It also pins the `output_kind` discriminator as a
//! `const`, which the schemars derive cannot do from a plain `&'static str`
//! field. Both differences are measured in `tests/measure.rs`.

use serde_json::{Value, json};

use crate::output::init_example;

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

/// The explicit field table — the SERIALIZED fields only. In production this is
/// exactly what the macro would emit from the annotated struct: one row per
/// serialized field. `trust`/`verification` is `#[serde(skip_serializing)]`, so
/// the wire bytes never carry it and the emitter omits it — no phantom property.
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
