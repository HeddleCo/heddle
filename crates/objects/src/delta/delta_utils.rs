// SPDX-License-Identifier: Apache-2.0
//! Delta utility helpers.

use super::delta_encoder::DeltaEncoder;

pub fn compute_delta(base: &[u8], target: &[u8]) -> Option<(Vec<u8>, f64)> {
    if target.is_empty() {
        return None;
    }

    let delta = DeltaEncoder::encode(base, target);
    let ratio = delta.len() as f64 / target.len() as f64;

    if ratio < 0.9 {
        Some((delta, ratio))
    } else {
        None
    }
}