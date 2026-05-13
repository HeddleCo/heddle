// SPDX-License-Identifier: Apache-2.0
//! Verification metadata for states.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Verification information for a state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Verification {
    /// Whether tests passed.
    pub tests_passed: Option<bool>,
    /// Number of failed tests.
    pub tests_failed: Option<u32>,
    /// Test coverage percentage.
    pub coverage_pct: Option<f32>,
    /// Change in coverage from parent.
    pub coverage_delta: Option<f32>,
    /// Number of lint warnings.
    pub lint_warnings: Option<u32>,
    /// Custom verification data.
    #[serde(default)]
    pub custom: BTreeMap<String, serde_json::Value>,
}

impl Verification {
    /// Create empty verification.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set tests passed.
    pub fn with_tests_passed(mut self, passed: bool) -> Self {
        self.tests_passed = Some(passed);
        self
    }

    /// Set test failures.
    pub fn with_tests_failed(mut self, failed: u32) -> Self {
        self.tests_failed = Some(failed);
        self
    }

    /// Check if any verification data is present.
    pub fn is_empty(&self) -> bool {
        self.tests_passed.is_none()
            && self.tests_failed.is_none()
            && self.coverage_pct.is_none()
            && self.coverage_delta.is_none()
            && self.lint_warnings.is_none()
            && self.custom.is_empty()
    }

    pub(crate) fn hash_len(&self) -> usize {
        let mut len = 0;

        len += 1 + self.tests_passed.map(|_| 1).unwrap_or(0);
        len += 1 + self.tests_failed.map(|_| 4).unwrap_or(0);
        len += 1 + self.coverage_pct.map(|_| 4).unwrap_or(0);
        len += 1 + self.coverage_delta.map(|_| 4).unwrap_or(0);
        len += 1 + self.lint_warnings.map(|_| 4).unwrap_or(0);

        len += 4;
        for (key, value) in &self.custom {
            let value_bytes = serde_json::to_vec(value).unwrap_or_default();
            len += 4 + key.len();
            len += 4 + value_bytes.len();
        }

        len
    }

    pub(crate) fn update_hasher(&self, hasher: &mut blake3::Hasher) {
        let tests_passed = self.tests_passed.map(u8::from);
        write_optional_u8(hasher, tests_passed);
        write_optional_u32(hasher, self.tests_failed);
        write_optional_f32(hasher, self.coverage_pct);
        write_optional_f32(hasher, self.coverage_delta);
        write_optional_u32(hasher, self.lint_warnings);

        let custom_len = self.custom.len() as u32;
        hasher.update(&custom_len.to_le_bytes());
        for (key, value) in &self.custom {
            let key_bytes = key.as_bytes();
            let value_bytes = serde_json::to_vec(value).unwrap_or_default();

            hasher.update(&(key_bytes.len() as u32).to_le_bytes());
            hasher.update(key_bytes);

            hasher.update(&(value_bytes.len() as u32).to_le_bytes());
            hasher.update(&value_bytes);
        }
    }
}

fn write_optional_u8(hasher: &mut blake3::Hasher, value: Option<u8>) {
    match value {
        Some(v) => {
            hasher.update(&[1]);
            hasher.update(&[v]);
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn write_optional_u32(hasher: &mut blake3::Hasher, value: Option<u32>) {
    match value {
        Some(v) => {
            hasher.update(&[1]);
            hasher.update(&v.to_le_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn write_optional_f32(hasher: &mut blake3::Hasher, value: Option<f32>) {
    match value {
        Some(v) => {
            hasher.update(&[1]);
            hasher.update(&v.to_le_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}