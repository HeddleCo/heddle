// SPDX-License-Identifier: Apache-2.0
//! Local CI check executor harvested from HeddleCo/treadle.

mod ansi;
mod body;
mod cache;
mod classify;
mod env;
mod exec;
mod model;
mod proc_group;
mod process;
mod result;
mod result_cache;
mod service;

pub use ansi::strip_ansi;
pub use cache::{CACHE_ENV_PREFIX, CachePathError, PreparedCaches, prepare_caches, save_caches};
pub use classify::{Disposition, EXCERPT_CAP_BYTES, classify, extract_excerpt};
pub use env::{BASE_ALLOWLIST, GIT_IDENTITY_EMAIL, GIT_IDENTITY_NAME, HermeticEnv};
pub use exec::{run_checks, run_checks_with};
pub use model::{AttemptRecord, CheckResult, ExecutionContext, RunControls, RunOptions};
pub use proc_group::ProcGroupRegistry;
pub use result_cache::{
    CacheKey, FsResultCache, MemoryResultCache, RESULT_CACHE_SCHEMA_VERSION, ResultCache,
    ResultCacheEntry, ResultCacheError, SpotCheck, SpotCheckDivergence, evidence_digest,
};
pub use service::{
    CommandOutcome, CommandRunner, DockerProvider, FakeProvider, NoopProvider, RealCommandRunner,
    RunningServices, ServiceError, ServiceProvider,
};
