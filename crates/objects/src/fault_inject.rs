// SPDX-License-Identifier: Apache-2.0
//! Deterministic fault-injection points for crash-recovery tests.
//!
//! W2b shipped the rollback machinery — atomic mapping persistence,
//! mirror Drop guard, HEAD/index restore on failure — but until we
//! actually crash the process between the load-bearing writes, the
//! rollback paths only have unit-test coverage of the *helpers*, not
//! of the *recovery contract itself*. The integration story
//! ("crashing here doesn't corrupt the Git Projection Mapping") was unverified.
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

// Test-only in-process override (avoids `OnceLock` + env pollution).
#[cfg(test)]
thread_local! {
    static TEST_FAULT_POINTS: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

fn env_active_points() -> &'static Option<Vec<String>> {
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

fn point_is_active(name: &str) -> bool {
    #[cfg(test)]
    {
        let override_hit = TEST_FAULT_POINTS.with(|cell| {
            cell.borrow()
                .as_ref()
                .map(|points| points.iter().any(|active| active == name))
        });
        // Some(true/false) = override active; None = fall through to env cache.
        if let Some(active) = override_hit {
            return active;
        }
    }
    env_active_points()
        .as_ref()
        .is_some_and(|points| points.iter().any(|active| active == name))
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
    if point_is_active(name) {
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
    if point_is_active(name) {
        return Err(std::io::Error::other(format!(
            "HEDDLE_FAULT_INJECT: failing at checkpoint `{name}` (intentional)"
        )));
    }
    Ok(())
}

/// Run `f` with in-process fault checkpoints active (test only).
///
/// Prefer this over mutating `HEDDLE_FAULT_INJECT` in unit tests: the env
/// parse is memoised in a process-global `OnceLock` and cannot be safely
/// flipped under parallel `cargo test`.
#[cfg(test)]
pub fn with_fault_points<R>(points: &[&str], f: impl FnOnce() -> R) -> R {
    TEST_FAULT_POINTS.with(|cell| {
        *cell.borrow_mut() = Some(points.iter().map(|s| (*s).to_owned()).collect());
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    TEST_FAULT_POINTS.with(|cell| {
        *cell.borrow_mut() = None;
    });
    match result {
        Ok(v) => v,
        Err(payload) => std::panic::resume_unwind(payload),
    }
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
