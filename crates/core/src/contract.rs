// SPDX-License-Identifier: Apache-2.0
//! Typed machine-output contracts for embeddable Heddle reports.

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

/// Stable contract metadata carried beside a typed Heddle report.
#[derive(Debug, Clone, Copy)]
pub struct ReportContract {
    /// Stable schema identifier for this report shape.
    pub schema_name: &'static str,
    /// Machine-readable stream/container kind emitted for this report.
    pub machine_output_kind: MachineOutputKind,
    /// Stable output discriminator emitted in the report payload.
    pub output_discriminator: OutputDiscriminator,
    /// Generate the JSON Schema for this report shape.
    pub schema: fn() -> Value,
}

/// A stable field/value discriminator in a machine-readable report payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputDiscriminator {
    pub field: &'static str,
    pub value: &'static str,
}

/// The machine-readable output container a report is serialized into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineOutputKind {
    /// A single JSON object.
    Json,
    /// Newline-delimited JSON records.
    JsonLines,
    /// Either a single JSON object or newline-delimited JSON records.
    JsonOrJsonLines,
}

/// A typed report that exposes its stable machine-output contract.
pub trait HeddleReport: Serialize + JsonSchema {
    const CONTRACT: ReportContract;
}

/// Generate a JSON Schema value for a report type.
pub fn schema_for_report<T>() -> Value
where
    T: JsonSchema,
{
    serde_json::to_value(schemars::schema_for!(T))
        .unwrap_or_else(|_| serde_json::json!({ "type": "object" }))
}
