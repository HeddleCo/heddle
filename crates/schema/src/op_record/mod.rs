// SPDX-License-Identifier: Apache-2.0
//! Versioned operation-record schema.

mod codec;
mod types;

pub use codec::{
    CURRENT_OP_RECORD_SCHEMA_VERSION, decode_current_record, encode_current_record,
    validate_op_record_schema_version,
};
pub use types::*;
