// SPDX-License-Identifier: Apache-2.0
//! Canonical `TreadleDefinition` loading for local Heddle CI.
//!
//! The on-disk contract is protobuf, not TOML:
//! - `.heddle/treadle.definition.bin` — canonical `TreadleDefinition` bytes
//! - `.heddle/treadle.lock.json` — `format_version` + hex BLAKE3 `definition_digest`
//!
//! `heddle ci run --local` decodes with the api canonical reader, requires
//! the lockfile, then maps each `TreadleCheck` onto the engine. The bin + lock
//! pair is SDK compile output (`emitPipeline` / `treadle-compile`). These
//! helpers are not a Rust SDK; authoring for real pipelines lives in the
//! language SDKs.

mod authoring;
mod load;
mod model;

pub use authoring::{
    TREADLE_LOCK_FORMAT, argv_check, canonical_definition, definition, host_pipeline_fixture,
    host_pipeline_with_required_failure, host_target_environment, job, lock_json,
    non_canonical_bytes, pipeline,
};
pub use load::{
    ConfigError, DEFAULT_DEFINITION_FILE, DEFAULT_LOCK_FILE, LoadedDefinition, TreadleLockfile,
    admit_host_exec, definition_digest, host_oci_platform, load, load_lock_file, lock_path,
    read_lock, verify_lock,
};
pub use model::{
    Check, CheckClass, CiConfig, DEFAULT_TIMEOUT_SECS, Retry, Service, Trigger,
    cache_path_is_worktree_relative,
};
