// SPDX-License-Identifier: Apache-2.0
//! Delta compression for efficient object transfer.

mod delta_decoder;
mod delta_encoder;
mod delta_utils;

#[cfg(test)]
mod delta_tests;

pub use delta_decoder::{DeltaDecoder, DeltaError, MAX_DELTA_OUTPUT_SIZE};
pub use delta_encoder::DeltaEncoder;
pub use delta_utils::compute_delta;
