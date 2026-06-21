// SPDX-License-Identifier: Apache-2.0
//! Shared CLI output mode values.

use clap::ValueEnum;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    Json,
    // JSON, but only the decision-surface fields (heddle#470). Renders as
    // `--output json-compact` on the CLI. Deliberately NOT a doc comment:
    // a per-value help string forces clap's spaced long-help layout onto
    // every command (the value list is rendered with the global --output
    // arg), re-bloating all 100+ helps; the format semantics live in
    // `heddle help output-formats` instead (heddle#652).
    JsonCompact,
    Text,
}
