// SPDX-License-Identifier: Apache-2.0
//! Developer-facing performance profiling helpers.
//!
//! The profile surface is intentionally env-gated so the public CLI
//! stays focused. `HEDDLE_PROFILE=1` writes human-readable timings to
//! stderr; stdout remains reserved for normal text/JSON command output.

use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct ProfileField {
    pub name: &'static str,
    pub value: u128,
}

impl ProfileField {
    pub fn millis(name: &'static str, value_ms: u128) -> Self {
        Self {
            name,
            value: value_ms,
        }
    }

    pub fn duration(name: &'static str, value: Duration) -> Self {
        Self {
            name,
            value: value.as_millis(),
        }
    }

    pub fn count(name: &'static str, value: impl Into<u128>) -> Self {
        Self {
            name,
            value: value.into(),
        }
    }
}

pub fn profile_enabled() -> bool {
    std::env::var("HEDDLE_PROFILE")
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            !matches!(normalized.as_str(), "" | "0" | "false" | "no" | "off")
        })
        .unwrap_or(false)
}

pub fn emit_profile(command: &str, fields: &[ProfileField]) {
    if !profile_enabled() {
        return;
    }

    eprintln!("heddle profile:");
    eprintln!("  command: {command}");
    for field in fields {
        eprintln!("  {}: {}", field.name, field.value);
    }
}
