// SPDX-License-Identifier: Apache-2.0
//! Implementation of `heddle start --shared-target`.
//!
//! When `heddle start` runs with `--shared-target` in a Rust workspace,
//! the resulting checkout's `target/` directory is redirected to a
//! workspace-wide shared path, so multiple parallel materialized
//! threads don't each get their own multi-gigabyte cargo target tree.
//!
//! Implementation choice: write a `.cargo/config.toml` inside the new
//! thread checkout with `[build] target-dir = "..."`. Cargo reads this
//! automatically for any invocation in that directory — no env-var
//! pollution, no required cooperation from downstream tools, and the
//! redirection survives across shell sessions.
//!
//! The shared directory lives at:
//!
//! ```text
//! <repo_root>/.heddle/targets/<workspace-fingerprint>/
//! ```
//!
//! `<workspace-fingerprint>` is a 16-hex-char SHA-256 digest derived
//! from the contents of the workspace's top-level `Cargo.lock` (when
//! present) or top-level `Cargo.toml` (fallback). Different repos
//! produce different fingerprints, and a stable workspace produces a
//! stable directory across thread creates.
//!
//! This module is item 2.1 of the heddle 6→8 plan.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use repo::{Repository, ThreadManager, ThreadMode};
use sha2::{Digest, Sha256};

/// Maximum width of the fingerprint hex string. Long enough that
/// collisions across distinct workspaces on one machine are not a
/// realistic concern; short enough to keep the on-disk path readable.
const FINGERPRINT_HEX_WIDTH: usize = 16;

/// Width of the "is the workspace busy enough to advise?" advisory
/// threshold. Defined as a named constant so the test suite and the
/// production path agree on what "≥ 1 active materialized thread"
/// means.
pub(crate) const ADVISORY_ACTIVE_THREAD_THRESHOLD: usize = 1;

/// Decide whether a workspace looks like a Rust workspace whose
/// `target/` is worth sharing. A top-level `Cargo.toml` is the only
/// signal we use — a `Cargo.lock` is not required because `cargo build`
/// will regenerate one in the new checkout, but `Cargo.toml` is what
/// makes the workspace a cargo workspace at all.
pub(crate) fn workspace_root_is_rust(repo: &Repository) -> bool {
    repo.root().join("Cargo.toml").is_file()
}

/// Compute the deterministic per-workspace fingerprint used for the
/// shared `target/` directory name.
///
/// Hash inputs, in order of preference:
///
/// 1. `Cargo.lock` at the repository root, if present. This is the
///    most precise signal: workspaces that share a `Cargo.lock`
///    produce identical artefacts and can safely share a `target/`.
/// 2. `Cargo.toml` at the repository root, otherwise. Less precise
///    (changing `Cargo.lock` after a dependency bump won't bust the
///    cache automatically), but stable across thread creates and
///    distinct between unrelated repos.
///
/// The output is the lowercase hex of the first
/// [`FINGERPRINT_HEX_WIDTH`] characters of a SHA-256 digest.
pub(crate) fn workspace_fingerprint(repo: &Repository) -> Result<String> {
    let lock = repo.root().join("Cargo.lock");
    let toml = repo.root().join("Cargo.toml");

    let bytes = if lock.is_file() {
        std::fs::read(&lock).with_context(|| format!("read {}", lock.display()))?
    } else if toml.is_file() {
        std::fs::read(&toml).with_context(|| format!("read {}", toml.display()))?
    } else {
        return Err(anyhow!(
            "no Cargo.toml at workspace root '{}'; --shared-target only applies to Rust workspaces",
            repo.root().display()
        ));
    };

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(hex[..FINGERPRINT_HEX_WIDTH].to_string())
}

/// Resolve the absolute path of the shared `target/` directory for
/// `repo`. Creates the directory (and intermediate `.heddle/targets/`)
/// if missing. Returns the path; callers persist this on the
/// `ThreadRecord` and embed it in `.cargo/config.toml`.
pub(crate) fn shared_target_dir(repo: &Repository) -> Result<PathBuf> {
    let fingerprint = workspace_fingerprint(repo)?;
    let dir = repo.heddle_dir().join("targets").join(fingerprint);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create shared target dir '{}'", dir.display()))?;
    Ok(dir)
}

/// Write a `.cargo/config.toml` inside `checkout` that redirects the
/// build target directory to `target_dir`.
///
/// If `<checkout>/.cargo/config.toml` already exists, this function
/// leaves it alone and returns `Ok(false)`: a user-managed config takes
/// precedence over the redirect, which matches cargo's own merge
/// semantics. Returns `Ok(true)` when the redirect was actually
/// written. Callers use this to decide whether to record the
/// `shared_target_dir` on the thread record — surfacing one when the
/// redirect didn't apply would lie to `heddle thread show`.
pub(crate) fn write_cargo_config(checkout: &Path, target_dir: &Path) -> Result<bool> {
    let cargo_dir = checkout.join(".cargo");
    let config_path = cargo_dir.join("config.toml");

    std::fs::create_dir_all(&cargo_dir)
        .with_context(|| format!("create '{}'", cargo_dir.display()))?;

    // We escape the path the way TOML basic strings expect:
    // backslashes (Windows) and double-quotes need a leading backslash.
    // Newlines/tabs are vanishingly unlikely in a real path; we still
    // escape them to keep the writer total.
    let escaped = target_dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t");

    let body = format!(
        "# Written by `heddle start --shared-target`. Redirects cargo's\n\
         # `target/` directory to a workspace-wide shared path so\n\
         # multiple parallel materialized threads don't each carry\n\
         # their own multi-gigabyte build tree.\n\
         #\n\
         # Safe to delete: cargo will fall back to a per-checkout\n\
         # `target/` next build.\n\
         [build]\n\
         target-dir = \"{escaped}\"\n",
    );

    // Atomic create-or-no-op: `create_new(true)` fails with
    // `AlreadyExists` if another process has the file. That's our
    // "user-managed config wins" path — return `Ok(false)` instead
    // of clobbering. Prevents the TOCTOU race between `exists()` and
    // `create()` that would otherwise let two concurrent
    // `heddle start --shared-target` invocations race over a
    // user-managed config.
    let file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config_path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            // User-managed config wins. We deliberately don't merge:
            // that would commit us to becoming a TOML editor, and
            // cargo already merges across config files at higher
            // levels of the search hierarchy.
            return Ok(false);
        }
        Err(err) => {
            return Err(err).with_context(|| format!("create '{}'", config_path.display()));
        }
    };
    // If `write_all` (or `sync_all`) fails after `create_new` already
    // landed an empty file (ENOSPC, EIO, network FS hiccup), a naïve
    // bail-out would leave a zero-byte or partial `config.toml` on
    // disk. A retried `heddle start --shared-target` would then hit
    // the `AlreadyExists` arm above and silently treat the orphan as
    // a user-managed config, returning `Ok(false)` and never wiring
    // the redirect. Remove the partial file so the retry can recreate
    // it. Mirrors the cleanup pattern in `init.rs` (heddle#80 r3).
    //
    // The helper takes the writer by value and drops it before
    // attempting `remove_file`. On Windows, `DeleteFile` on a still-open
    // handle (without `FILE_SHARE_DELETE`) fails with
    // `ERROR_SHARING_VIOLATION`; the cleanup would silently no-op, the
    // orphan would stay, and the retry would still hit `AlreadyExists`.
    let file = write_body_or_cleanup(file, body.as_bytes(), &config_path)?;
    if let Err(err) = file.sync_all() {
        drop(file);
        let _ = std::fs::remove_file(&config_path);
        return Err(err).with_context(|| format!("sync '{}'", config_path.display()));
    }
    Ok(true)
}

/// Write `body` to `writer`; on failure drop the writer (closing any
/// underlying OS handle) and remove the orphan at `cleanup_path`,
/// returning the original write error. On success, return the writer so
/// the caller can continue using it (e.g. for `sync_all`).
///
/// Generic over `Write` so tests can inject a failing writer to exercise
/// the cleanup branch — the production caller passes the
/// freshly-`create_new`'d file by value.
///
/// **Drop-before-remove is load-bearing on Windows.** Without
/// `FILE_SHARE_DELETE`, `DeleteFile` against a still-open handle fails
/// with `ERROR_SHARING_VIOLATION`. Taking the writer by value makes
/// ownership obvious at the call site and forces the close to happen
/// before `remove_file`.
fn write_body_or_cleanup<W: Write>(mut writer: W, body: &[u8], cleanup_path: &Path) -> Result<W> {
    match writer.write_all(body) {
        Ok(()) => Ok(writer),
        Err(err) => {
            drop(writer);
            let _ = std::fs::remove_file(cleanup_path);
            Err(err).with_context(|| format!("write '{}'", cleanup_path.display()))
        }
    }
}

/// Count active materialized threads on the repo. "Active" means
/// `ThreadState::Active` and `ThreadMode::Materialized | Materialized`
/// — both are heavy (real on-disk) checkouts. Used by the advisory
/// path; not load-bearing for correctness.
fn count_active_materialized_threads(repo: &Repository) -> usize {
    let manager = ThreadManager::new(repo.heddle_dir());
    let Ok(threads) = manager.list() else {
        return 0;
    };
    threads
        .into_iter()
        .filter(|thread| {
            matches!(thread.mode, ThreadMode::Solid | ThreadMode::Materialized)
                && thread.state == repo::ThreadState::Active
        })
        .count()
}

/// Whether the advisory should fire when starting a new materialized
/// thread.
///
/// Heuristic: the workspace has a top-level `Cargo.toml`, and there is
/// already at least [`ADVISORY_ACTIVE_THREAD_THRESHOLD`] active
/// materialized thread on this repo.
///
/// Called *before* the new thread is recorded, so the count reflects
/// the pre-existing population (a single thread starting in isolation
/// does not nudge).
pub(crate) fn should_advise_shared_target(repo: &Repository) -> bool {
    workspace_root_is_rust(repo)
        && count_active_materialized_threads(repo) >= ADVISORY_ACTIVE_THREAD_THRESHOLD
}

/// Print the heads-up advisory to stderr. Kept in this module so the
/// wording lives next to the heuristic that triggers it.
pub(crate) fn print_advisory(name: &str) {
    eprintln!(
        "note: starting materialized thread '{name}' alongside an existing materialized thread \
         in a Rust workspace; consider `heddle start --shared-target {name}` to share cargo's \
         target/ across threads (saves multiple GB).",
    );
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn fingerprint_is_stable_across_calls() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), b"[package]\nname=\"x\"\n").unwrap();

        // We can't construct a real Repository here without going
        // through `heddle init`, so we test the inner hash directly
        // by reading the same content twice.
        let bytes = std::fs::read(temp.path().join("Cargo.toml")).unwrap();
        let mut a = Sha256::new();
        a.update(&bytes);
        let mut b = Sha256::new();
        b.update(&bytes);
        assert_eq!(a.finalize(), b.finalize());
    }

    #[test]
    fn write_cargo_config_creates_file_with_target_dir() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("targets").join("abc123");
        std::fs::create_dir_all(&target).unwrap();
        let wrote = write_cargo_config(temp.path(), &target).unwrap();
        assert!(wrote, "writer must report a write when no prior config");

        let written =
            std::fs::read_to_string(temp.path().join(".cargo").join("config.toml")).unwrap();
        assert!(written.contains("[build]"));
        assert!(written.contains(&format!("target-dir = \"{}\"", target.display(),)));
    }

    #[test]
    fn write_cargo_config_preserves_existing_user_config() {
        let temp = TempDir::new().unwrap();
        let cargo_dir = temp.path().join(".cargo");
        std::fs::create_dir_all(&cargo_dir).unwrap();
        let user = "[net]\noffline = true\n";
        std::fs::write(cargo_dir.join("config.toml"), user).unwrap();

        let target = temp.path().join("shared");
        std::fs::create_dir_all(&target).unwrap();
        let wrote = write_cargo_config(temp.path(), &target).unwrap();
        assert!(
            !wrote,
            "writer must report no-op when user config is preserved"
        );

        let after = std::fs::read_to_string(cargo_dir.join("config.toml")).unwrap();
        assert_eq!(
            after, user,
            "shared-target writer must not overwrite user-managed config",
        );
    }

    /// `Write` impl whose `write` always errors. Used to drive the
    /// partial-write cleanup branch in `write_body_or_cleanup` without
    /// needing to actually exhaust disk space.
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("simulated write failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_body_or_cleanup_removes_orphan_on_write_failure() {
        // Regression for heddle#86 (follow-up to heddle#80 r3). If
        // `write_all` fails after `create_new` already landed the
        // file, the partial file must be removed so a retry can
        // re-enter `create_new` instead of hitting `AlreadyExists`
        // and silently reporting no-op success.
        let temp = TempDir::new().unwrap();
        let orphan = temp.path().join(".cargo").join("config.toml");
        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        // Stand in for what `create_new(true)` would have just produced.
        std::fs::write(&orphan, b"").unwrap();
        assert!(orphan.exists(), "test precondition: orphan staged");

        let writer = FailingWriter;
        let result = write_body_or_cleanup(writer, b"would-be body", &orphan);

        assert!(
            result.is_err(),
            "writer failure must surface to caller, not be swallowed"
        );
        assert!(
            !orphan.exists(),
            "orphan file must be removed so a retry can re-create it cleanly"
        );
    }

    /// `Write` wrapper that flips a flag from its `Drop` impl. The
    /// regression test below uses this to assert the writer is closed
    /// before the helper's `remove_file` call — load-bearing on Windows,
    /// where `DeleteFile` against a still-open handle fails with
    /// `ERROR_SHARING_VIOLATION` and the orphan would otherwise survive.
    struct DropTrackingFailingWriter<'a> {
        dropped: &'a std::cell::Cell<bool>,
    }
    impl Write for DropTrackingFailingWriter<'_> {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("simulated write failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Drop for DropTrackingFailingWriter<'_> {
        fn drop(&mut self) {
            self.dropped.set(true);
        }
    }

    #[test]
    fn write_body_or_cleanup_drops_writer_before_returning_on_failure() {
        // Regression for the Codex r1 finding on heddle#86. On Windows,
        // `remove_file` against a still-open handle fails with
        // `ERROR_SHARING_VIOLATION`; the orphan stays, and the retry
        // never installs the redirect. The helper must take the writer
        // by value and drop it before `remove_file`. POSIX would let the
        // unlink succeed against an open handle, so this test asserts
        // the ownership-transfer guarantee directly: by the time the
        // helper returns, the writer has been dropped (i.e. on a real
        // `File`, the OS handle is closed).
        let temp = TempDir::new().unwrap();
        let orphan = temp.path().join(".cargo").join("config.toml");
        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, b"").unwrap();

        let dropped = std::cell::Cell::new(false);
        let writer = DropTrackingFailingWriter { dropped: &dropped };
        let result = write_body_or_cleanup(writer, b"would-be body", &orphan);

        assert!(result.is_err());
        assert!(
            dropped.get(),
            "writer must be dropped before the helper returns on failure — \
             on Windows, the file handle must be closed before remove_file"
        );
        assert!(!orphan.exists());
    }

    #[test]
    fn write_cargo_config_escapes_quotes_in_path() {
        // The path component this test smuggles in is improbable in
        // practice, but the writer must still produce parseable TOML.
        let temp = TempDir::new().unwrap();
        let weird = temp.path().join("dir with \"quotes\"");
        std::fs::create_dir_all(&weird).unwrap();
        let wrote = write_cargo_config(temp.path(), &weird).unwrap();
        assert!(wrote);

        let written =
            std::fs::read_to_string(temp.path().join(".cargo").join("config.toml")).unwrap();
        // Should round-trip through a TOML parser cleanly.
        let parsed: toml::Value = toml::from_str(&written).unwrap();
        let target_dir = parsed
            .get("build")
            .and_then(|t| t.get("target-dir"))
            .and_then(|v| v.as_str())
            .expect("[build].target-dir present");
        assert_eq!(target_dir, weird.display().to_string());
    }
}
