// SPDX-License-Identifier: Apache-2.0
//! Versioned operation-record schema.

mod codec;
mod types;

pub use codec::{
    LATEST_RECORD_SCHEMA_VERSION, OpRecordSchemaVersion, candidate_versions_newest_first,
    decode_versioned_record, encode_latest_record, schema_version_from_u32, tests_support,
};
pub use types::*;
