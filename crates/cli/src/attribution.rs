// SPDX-License-Identifier: Apache-2.0

/// Treat the `"unknown"` harness placeholder and empty/whitespace
/// strings as absent so they don't beat real attribution values in
/// precedence chains.
pub(crate) fn clean_attribution_value(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("unknown") {
        None
    } else {
        Some(value)
    }
}
