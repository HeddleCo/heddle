// SPDX-License-Identifier: Apache-2.0
//! `heddle-fuse-worker` — out-of-process FUSE callback handler.
//!
//! See `crates/mount/src/worker.rs` for the architecture; this file
//! is the thinnest possible `main` shim — argv → [`mount::worker::run_worker`].
//!
//! Co-located in the CLI crate so `cargo install --path crates/cli`
//! (and `cargo install heddle` from crates.io) ships the worker
//! binary alongside `heddle` itself. Without that co-location the
//! sibling lookup in `mount::worker::default_worker_binary` finds
//! nothing on a standard install and the mount lifecycle silently
//! falls back to NFS (heddle#190 r4 / Codex PR #225 P1).
//!
//! Per-function cfg (not crate-level) so the binary still compiles
//! on macOS/Windows when someone runs `cargo install heddle-cli
//! --features mount` there: `required-features = ["mount"]` selects
//! the `[[bin]]` on all three platforms, so `main` must exist on
//! every target the `mount` feature compiles for. The non-Linux
//! main is a stub that prints a usable error and exits 2 — the
//! supervisor on those platforms uses the in-process mount path
//! (heddle#190 r5 / Codex PR #225 P1).
//!
//! Cross-platform correctness of this pattern is verified by code
//! review + `cargo check` at PR time, not by a CI cross-compile
//! job: provisioning mingw-w64 + cross-compiling `aws-lc-sys`
//! (transitive via `rustls`) added several minutes to every CI run
//! to verify a handful of lines of trivial cfg, which is out of
//! proportion to the value (heddle#190 r7).

use std::process::ExitCode;

#[cfg(target_os = "linux")]
use mount::worker::{run_worker, WorkerArgs};

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    // Tracing subscriber: ENV-controlled like the rest of heddle.
    // `RUST_LOG=heddle=debug heddle-fuse-worker ...` works the same
    // way it does for the CLI. We deliberately install the same
    // shape as `tracing_subscriber::fmt::init` (env filter +
    // human-readable fmt to stderr) rather than the daemon's
    // structured-JSON output: the worker's logs are read by hand
    // when a mount has gone wrong, not aggregated by an LB.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("heddle=info,mount=info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    // Argv parsing. We skip argv[0] and feed the rest through
    // `WorkerArgs::parse`. Errors here are config bugs in the
    // supervisor, not user mistakes — print the usage so a
    // hand-test that runs the binary directly gets a hint.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match WorkerArgs::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("heddle-fuse-worker: {e}");
            eprintln!(
                "usage: heddle-fuse-worker \
                 --repo-root <path> \
                 --thread-id <id> \
                 --mountpoint <path> \
                 --ipc-fd <n>"
            );
            return ExitCode::from(2);
        }
    };

    match run_worker(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Print the full chain on stderr so a tail-log gives
            // the operator the failing context. Exit code 1 is
            // the supervisor's signal "this was a normal-shaped
            // failure", distinct from the 101 a panic produces.
            eprintln!("heddle-fuse-worker: {e:#}");
            ExitCode::FAILURE
        }
    }
}

// Non-Linux stub. The crash-isolation worker is FUSE-specific and
// only meaningful on Linux; FSKit and ProjFS run their callbacks
// in-process via their own platform sandboxes. We still ship the
// binary on macOS/Windows so `cargo install` produces the same
// file set everywhere (avoids "missing binary" surprises in
// scripts and packagers); invoking it exits with a clear error.
#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!(
        "heddle-fuse-worker: only supported on Linux; \
         use the in-process mount path on macOS/Windows"
    );
    ExitCode::from(2)
}
