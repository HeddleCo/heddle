// SPDX-License-Identifier: Apache-2.0
//! `heddle-fuse-worker` — out-of-process FUSE callback handler.
//!
//! See `crates/mount/src/worker.rs` for the architecture; this file
//! is the thinnest possible `main` shim — argv → [`mount::worker::run_worker`].
//!
//! Linux-only. The crate's `Cargo.toml` gates the binary behind
//! `required-features = ["fuse"]`, and the file itself is
//! `cfg(target_os = "linux")` so non-Linux builds skip it cleanly.

#![cfg(all(target_os = "linux", feature = "fuse"))]

use std::process::ExitCode;

use mount::worker::{run_worker, WorkerArgs};

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
