// SPDX-License-Identifier: Apache-2.0
//! Per-test `HEDDLE_HOME` so the device catalog cannot leak across tests.
//!
//! `Repository::init*` registers every native spool in
//! `<heddle_home>/state/device-rpc/catalog.sqlite3`. Without isolation, the
//! 872 `heddle-repo` unit tests share one process-global home (`$HEDDLE_HOME`
//! or `$HOME/.heddle`) and later tests observe earlier catalog rows, watermarks,
//! and heads (heddle#1766).
//!
//! libtest spawns a dedicated thread named after the test even at
//! `--test-threads=1`, so a thread-local tempdir is per-test. Keying by the
//! thread name additionally refreshes the home if a worker thread is reused.

use std::cell::RefCell;
use std::path::PathBuf;

thread_local! {
    static HOME: RefCell<Option<(String, tempfile::TempDir)>> = const { RefCell::new(None) };
}

/// Point this test thread at a fresh `HEDDLE_HOME` before any catalog open.
///
/// Safe to call more than once in the same test: the same directory is reused
/// until a different test occupies the thread.
pub(crate) fn isolate_heddle_home() {
    HOME.with(|slot| {
        let key = test_key();
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|(existing, _)| existing == &key) {
            return;
        }
        *slot = Some((key, fresh_home()));
    });
}

pub(crate) fn heddle_home_dir() -> PathBuf {
    isolate_heddle_home();
    HOME.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("isolate_heddle_home must install a home")
            .1
            .path()
            .to_path_buf()
    })
}

fn test_key() -> String {
    std::thread::current()
        .name()
        .unwrap_or("unnamed-heddle-repo-test")
        .to_string()
}

fn fresh_home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("heddle-repo-home-")
        .tempdir()
        .expect("isolate HEDDLE_HOME")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolate_heddle_home_is_stable_within_a_test_and_off_the_operator_home() {
        isolate_heddle_home();
        let first = heddle_home_dir();
        let second = heddle_home_dir();
        assert_eq!(first, second, "one test must keep one device catalog");
        assert!(
            first
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("heddle-repo-home-")),
            "isolated home should be a heddle-repo-home tempdir, got {}",
            first.display()
        );
        if let Some(home) = std::env::var_os("HOME") {
            assert_ne!(
                first,
                std::path::PathBuf::from(home).join(".heddle"),
                "tests must not share the operator device catalog"
            );
        }
        if let Some(explicit) = std::env::var_os("HEDDLE_HOME") {
            assert_ne!(
                first,
                std::path::PathBuf::from(explicit),
                "a process-global HEDDLE_HOME must not leak into this test"
            );
        }
    }
}
