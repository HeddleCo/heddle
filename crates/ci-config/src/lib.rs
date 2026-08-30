// SPDX-License-Identifier: Apache-2.0
//! Canonical `TreadleDefinition` loading for local Heddle CI.
//!
//! The on-disk contract is protobuf, not TOML:
//! - `.heddle/treadle.definition.bin` — canonical `TreadleDefinition` bytes
//! - `.heddle/treadle.lock.json` — `format_version` + hex BLAKE3 `definition_digest`
//!
//! `heddle ci run --local` decodes with the api canonical reader, checks the
//! lockfile when present, then maps each `TreadleCheck` onto the engine.

mod authoring;
mod load;
mod model;

pub use authoring::{
    TREADLE_LOCK_FORMAT, argv_check, canonical_definition, definition, host_target_environment,
    lock_json, non_canonical_bytes,
};
pub use load::{
    ConfigError, DEFAULT_DEFINITION_FILE, DEFAULT_LOCK_FILE, LoadedDefinition, TreadleLockfile,
    admit_host_exec, definition_digest, host_oci_platform, load, lock_path, read_lock, verify_lock,
};
pub use model::{Check, CheckClass, CiConfig, DEFAULT_TIMEOUT_SECS, Retry, Service, Trigger};
