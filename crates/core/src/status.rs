// SPDX-License-Identifier: Apache-2.0
//! Status facade types.

/// Inputs for computing `heddle status`.
///
/// `render_json` is a CLI-resolved output hint for now because the legacy
/// status computation uses it to decide whether to pay for the full thread walk.
/// The facade still returns typed data; the CLI remains responsible for actually
/// choosing and emitting text vs. JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusOptions {
    pub short: bool,
    pub render_json: bool,
    pub verbose: bool,
}
