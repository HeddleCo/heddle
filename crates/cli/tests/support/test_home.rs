// SPDX-License-Identifier: Apache-2.0
//! Heddle home isolation for fixtures run by nextest.

static ISOLATED: std::sync::Once = std::sync::Once::new();

thread_local! {
    static HOME: tempfile::TempDir = {
        let home = tempfile::Builder::new()
            .prefix("heddle-cli-home-")
            .tempdir()
            .expect("isolated CLI test home");
        // nextest runs one test per process; worker threads inherit this home.
        unsafe { std::env::set_var("HEDDLE_HOME", home.path()) };
        home
    };
}

pub(super) fn isolate_test_home() {
    // Legacy libtest can run multiple tests in-process. Leave its runner home
    // alone, and preserve the explicit homes of our isolated child fixtures.
    if std::env::var_os("NEXTEST").is_some()
        && std::env::var_os("CLI_ISOLATED_TEST_CASE").is_none()
        && std::env::var_os("HEDDLE_PRIVACY_TEST_CHILD").is_none()
    {
        ISOLATED.call_once(|| HOME.with(|_| {}));
    }
}
