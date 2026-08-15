// SPDX-License-Identifier: Apache-2.0
//! Content-addressed check-result cache keyed on `(env, inputs, definition)`.

mod entry;
mod key;
mod spot_check;
mod store;

use std::collections::BTreeMap;

use ci_config::Check;
use crypto::Conclusion;
pub use entry::{
    RESULT_CACHE_SCHEMA_VERSION, ResultCacheEntry, ResultCacheError, SpotCheckDivergence,
    evidence_digest,
};
pub use key::CacheKey;
pub use spot_check::SpotCheck;
pub use store::{FsResultCache, MemoryResultCache, ResultCache};

use crate::{
    exec::ResolvedRun,
    model::{CheckResult, ExecutionContext},
};

pub(crate) fn with_cache(
    check: &Check,
    context: &ExecutionContext,
    run: &ResolvedRun<'_>,
    key_environment: &BTreeMap<String, String>,
    run_fresh: impl FnOnce() -> CheckResult,
) -> Result<CheckResult, ResultCacheError> {
    let Some(cache) = run.result_cache else {
        return Ok(run_fresh());
    };
    let key = CacheKey::derive(key_environment, context);
    if let Some(entry) = cache.get(&key, &check.name)? {
        if run.spot_check.should_sample(&key, &check.name) {
            let fresh = run_fresh();
            entry.verify_fresh(&fresh)?;
        }
        return Ok(entry.into_check_result());
    }
    let fresh = run_fresh();
    if is_cacheable(&fresh) {
        cache.put(&ResultCacheEntry::from_result(&key, &check.name, &fresh))?;
    }
    Ok(fresh)
}

fn is_cacheable(result: &CheckResult) -> bool {
    !matches!(
        result.conclusion(),
        Conclusion::Skipped | Conclusion::Cancelled | Conclusion::InfraError
    )
}
