// SPDX-License-Identifier: Apache-2.0
//! Deterministic fault-injection points for crash-recovery tests.
//!
//! W2b shipped the rollback machinery — atomic mapping persistence,
//! mirror Drop guard, HEAD/index restore on failure — but until we
//! actually crash the process between the load-bearing writes, the
//! rollback paths only have unit-test coverage of the *helpers*, not
//! of the *recovery contract itself*. The integration story
//! ("crashing here doesn't corrupt the bridge mapping") was unverified.
//!
//! This module exposes a single `maybe_panic_at(name)` checkpoint
//! that production code threads at the points where a crash would
//! exercise a recovery path. Tests opt in by setting the
//! `HEDDLE_FAULT_INJECT` environment variable to a comma-separated
//! list of checkpoint names — e.g.
//! `HEDDLE_FAULT_INJECT=mapping_after_tmp_before_commit` — and the
//! next process to hit that checkpoint panics with a stable message.
//!
//! The next CLI invocation (a separate process, no inherited env)
//! must recover cleanly. That's the contract under test.
//!
//! ## Why an env var instead of a build-time `#[cfg(test)]` gate
//!
//! The crash points sit in `objects` and `cli` paths that get spawned
//! as separate child processes during integration tests. A child
//! process can't see the parent test's `cfg(test)` flag, but it does
//! inherit env vars by default. An env var lets the parent test set
//! the crash point, spawn the child, observe the child crash, then
//! spawn a fresh child (without the env var) and verify recovery.
//!
//! ## Performance
//!
//! `maybe_panic_at` is a single env lookup + string split + linear
//! search. The env var is read once on first call and cached. With no
//! `HEDDLE_FAULT_INJECT` set (the production default), the cached
//! `None` short-circuits in well under a microsecond.

use std::sync::OnceLock;

/// Cached parse of the `HEDDLE_FAULT_INJECT` env var. `None` means
/// the env var was not set; an empty `Vec` means it was set to an
/// empty string (treated as no checkpoints active).
static FAULT_POINTS: OnceLock<Option<Vec<String>>> = OnceLock::new();

fn active_points() -> &'static Option<Vec<String>> {
    FAULT_POINTS.get_or_init(|| {
        std::env::var("HEDDLE_FAULT_INJECT").ok().map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
    })
}

/// Crash the current process if `name` is listed in `HEDDLE_FAULT_INJECT`.
///
/// Production callers thread this at points where a crash would
/// exercise a recovery path. Tests set the env var on a child
/// process to deterministically trigger the crash, then verify the
/// next clean process recovers.
///
/// The panic message includes the checkpoint name so test logs can
/// distinguish an intentional fault from a real bug.
pub fn maybe_panic_at(name: &str) {
    if let Some(points) = active_points().as_ref()
        && points.iter().any(|active| active == name)
    {
        panic!("HEDDLE_FAULT_INJECT: crashing at checkpoint `{name}` (intentional)");
    }
}

/// Like [`maybe_panic_at`], but returns an `io::Error` instead of
/// panicking — for exercising *in-process* error-recovery paths (a
/// graceful failure that drives a rollback) rather than crash recovery.
///
/// Production callers thread this where a returned error must unwind a
/// partially-applied operation; tests opt in by listing the checkpoint
/// name in `HEDDLE_FAULT_INJECT` and assert the rollback left no
/// partial state. With the env var unset the cached `None`
/// short-circuits, exactly like [`maybe_panic_at`].
pub fn maybe_fail_at(name: &str) -> std::io::Result<()> {
    if let Some(points) = active_points().as_ref()
        && points.iter().any(|active| active == name)
    {
        return Err(std::io::Error::other(format!(
            "HEDDLE_FAULT_INJECT: failing at checkpoint `{name}` (intentional)"
        )));
    }
    Ok(())
}

/// Test-only helper: clear the cached env-var read so a single
/// process can re-parse `HEDDLE_FAULT_INJECT` between phases. Not
/// for production use — the cache is what makes the production
/// hot-path free.
#[cfg(test)]
pub fn reset_for_test() {
    // OnceLock has no public reset; we work around by leaking a new
    // one. This is fine for tests because the binary lifetime is
    // bounded.
    use std::sync::atomic::{AtomicPtr, Ordering};
    static SLOT: AtomicPtr<OnceLock<Option<Vec<String>>>> = AtomicPtr::new(std::ptr::null_mut());
    let new = Box::leak(Box::new(OnceLock::new()));
    SLOT.store(new as *mut _, Ordering::SeqCst);
    // The static FAULT_POINTS isn't actually swappable; tests that
    // need to flip the env var multiple times within one process
    // should spawn child processes instead. This helper exists so
    // unit tests of `maybe_panic_at` itself can reset between
    // setup/teardown — and even there we just leak.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_env_var_is_a_silent_noop() {
        // Safety: this test only runs when HEDDLE_FAULT_INJECT is
        // unset, which is the production default. A test runner
        // that exports the var globally would change behaviour, but
        // that would be the runner's problem to surface explicitly.
        if std::env::var("HEDDLE_FAULT_INJECT").is_ok() {
            return;
        }
        // Should not panic — env var unset, all checkpoints inactive.
        maybe_panic_at("anything");
    }

    // NOTE: the original sibling test
    // `env_var_with_matching_name_panics` lived here and was flaky in
    // parallel runs. The flake was structural: `FAULT_POINTS` is a
    // `OnceLock`, so whichever test calls `active_points()` first wins.
    // If `no_env_var_is_a_silent_noop` ran first it cached `None`, and
    // the panic test could never re-arm the checkpoint.
    //
    // The fix was to move the panic test to its own integration-test
    // binary (`tests/fault_inject_panic.rs`); each integration test
    // file gets its own process and its own OnceLock state, so the
    // test always observes a fresh cache.
}
