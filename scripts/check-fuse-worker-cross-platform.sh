#!/usr/bin/env bash
# Cross-check that `heddle-fuse-worker` compiles on a non-Linux mount
# target. `cargo install heddle-cli --features mount` on macOS / Windows
# selects the `[[bin]]` whenever `required-features = ["mount"]` is met,
# so the binary's `main` must exist on every target the `mount` feature
# compiles for — not just Linux. A crate-level `#![cfg(target_os =
# "linux")]` removes `main` entirely on non-Linux and fails with E0601
# ("main function not found"); per-function cfg + a stub main is the
# fix (heddle#190 r5 / Codex PR #225 P1).
#
# We pick `x86_64-pc-windows-gnu` because the GNU toolchain cross-checks
# cleanly from a Linux host without needing the MSVC linker, and because
# the `mount` feature genuinely propagates on Windows (`mount?/projfs`
# pulls in the `windows` crate behind a target-cfg). The macOS targets
# would catch the same bug, but require Xcode tooling that GitHub-hosted
# Linux runners don't ship.
#
# Prereq: `x86_64-w64-mingw32-gcc` must be on PATH. `aws-lc-sys` (pulled
# in transitively via `rustls`) is a `*-sys` crate whose `build.rs`
# invokes the C compiler even under `cargo check`, so the cross-toolchain
# is required despite this being a check-only run. On Debian/Ubuntu:
# `sudo apt-get install -y --no-install-recommends mingw-w64`
# (heddle#190 r6).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

target="x86_64-pc-windows-gnu"
if ! rustup target list --installed | grep -qx "$target"; then
    rustup target add "$target"
fi

cargo check --locked --target "$target" \
    -p heddle-cli --features mount --bin heddle-fuse-worker
