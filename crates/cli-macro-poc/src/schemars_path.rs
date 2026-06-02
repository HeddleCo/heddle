// SPDX-License-Identifier: Apache-2.0
//! PATH A — schemars derive. One line at the call site once the macro emits the
//! derive; this is what heddle does today via the hand-written mirror.

use serde_json::Value;

use crate::output::InitOutput;

/// JSON Schema for `init --output json` via `schemars::schema_for!`, derived
/// directly on the real-shaped [`InitOutput`] (the shape heddle#205 would
/// migrate to). Because the real `trust` field is only `#[serde(skip_serializing)]`,
/// this schema carries a `verification` property the wire output never emits —
/// see `tests/measure.rs`.
pub fn schema() -> Value {
    let root = schemars::schema_for!(InitOutput);
    serde_json::to_value(&root).expect("schemars RootSchema serializes")
}
