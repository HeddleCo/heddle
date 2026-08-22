// SPDX-License-Identifier: Apache-2.0
//! Agent-facing session relay and harness probing.
//!
//! Extracted from the CLI's `harness` module to cut the cli ↔ harness
//! dependency cycle. This crate owns the agent plumbing: detecting which
//! coding agent wraps the process (`probe`), translating agent hook events
//! into Heddle sessions/progress/timeline operations (`relay`), and the
//! Claude Code hook behaviors (`claude_hook`).
//!
//! Dependency direction: `cli → agent-relay → {repo, objects, wire, verbs,
//! config, hosted-client, cli-render}` — acyclic because none of those
//! crates depend on the CLI. The two pieces that genuinely need CLI-owned
//! machinery (snapshot capture and worktree-target materialization) stay
//! behind the [`HarnessCliBridge`] port defined here and implemented in the
//! CLI, mirroring the `repo::lazy_hydrator` BlobHydrator precedent.

pub mod bridge;
mod claude_hook;
mod probe;
mod relay;

pub use bridge::{HarnessCliBridge, RelayCapture};
pub use probe::{HarnessProbeInput, HarnessProbeResult};
pub use relay::{
    HarnessBridgeRuntime, current_process_harness_hint, probe_current_process_harness,
    relay_harness_event,
};
