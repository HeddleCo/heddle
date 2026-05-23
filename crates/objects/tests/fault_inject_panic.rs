// SPDX-License-Identifier: Apache-2.0
//! Standalone integration test for the `HEDDLE_FAULT_INJECT` checkpoint.
//!
//! Lives in its own integration-test binary on purpose:
//! [`objects::fault_inject::active_points`] memoises the env-var read
//! through a `OnceLock`. Inside the same binary, whichever test calls
//! `active_points()` first wins, and the sibling unit test
//! `no_env_var_is_a_silent_noop` makes the cache observe an unset
//! env var when it runs first — leaving this test unable to re-arm
//! the checkpoint and flaking in parallel runs.
//!
//! Each integration-test file gets its own process and its own
//! OnceLock state, so this test always observes a fresh cache and
//! the panic fires deterministically.

#[test]
#[should_panic(expected = "HEDDLE_FAULT_INJECT: crashing at checkpoint")]
fn env_var_with_matching_name_panics() {
    // SAFETY: this is the only test in the binary; nothing else reads
    // or writes the env var concurrently.
    unsafe {
        std::env::set_var("HEDDLE_FAULT_INJECT", "test_panic_checkpoint");
    }
    objects::fault_inject::maybe_panic_at("test_panic_checkpoint");
}
