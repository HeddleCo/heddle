// SPDX-License-Identifier: Apache-2.0
//! Shared sanitized child environment for commands that spawn user
//! processes (`heddle run`, `heddle try`).
//!
//! Spawning a child with the parent's full environment leaks
//! Heddle-internal and sensitive variables — `GIT_DIR`, `GIT_WORK_TREE`,
//! `GIT_INDEX_FILE`, cloud credentials, etc. — into arbitrary user
//! commands. We instead `env_clear()` and rebuild from a minimal
//! explicit allowlist. A blocklist would only chase the next leaking
//! var; clearing the slate and opting variables back in closes the
//! whole class. This lives in one place so every spawn site shares the
//! same allowlist and can't drift.

/// The minimal environment a spawned child legitimately needs:
/// `PATH`/`HOME`/identity for the shell, locale for output. Everything
/// else (notably `GIT_*` and any inherited secrets) is dropped.
pub(crate) fn sanitized_child_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "PATH" | "HOME" | "USER" | "LOGNAME" | "SHELL" | "TMPDIR" | "TEMP" | "TMP" | "LANG"
            ) || key.starts_with("LC_")
        })
        .collect()
}
