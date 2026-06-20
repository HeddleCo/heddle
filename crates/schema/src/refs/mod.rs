// SPDX-License-Identifier: Apache-2.0
//! Persisted refs schema models.

mod packed_model;
mod reftable_model;

pub use packed_model::PackedRefsModel;
pub use reftable_model::{FOOTER_LEN, HEADER_LEN, MAGIC, ReftableError, ReftableModel};
