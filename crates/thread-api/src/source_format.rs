// SPDX-License-Identifier: Apache-2.0
//! Native source support is independent of signed-operation wire support.
//! Empty peer support means legacy-only; never rewrite an unsupported State.
pub use api::source_format::{
    NativeSourceFormatError, STATE_V6_ATTRIBUTION_V1, require_native_source_formats,
};

/// This implementation preserves format-6 States, HCS3 and required HAE1 closure.
pub const UNDERSTOOD_NATIVE_SOURCE_FORMATS: &[i32] = &[STATE_V6_ATTRIBUTION_V1];

#[cfg(feature = "replication")]
pub fn state_required_formats(state: &heddle_object_model::object::State) -> Vec<i32> {
    if state.attribution_evidence.is_some() {
        vec![STATE_V6_ATTRIBUTION_V1]
    } else {
        Vec::new()
    }
}

#[cfg(feature = "replication")]
pub fn operation_required_formats(
    operation: &heddle_object_model::object::thread_replication::ThreadOperation,
) -> heddle_object_model::error::Result<Vec<i32>> {
    Ok(operation
        .source_state()?
        .as_ref()
        .map(state_required_formats)
        .unwrap_or_default())
}
