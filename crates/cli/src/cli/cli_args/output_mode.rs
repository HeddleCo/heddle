// SPDX-License-Identifier: Apache-2.0
//! CLI-local parsing shape for shared output modes.

use cli_shared::OutputMode;

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CliOutputMode {
    Json,
    // JSON, but only the decision-surface fields (heddle#470). Keep this
    // comment non-doc so clap's compact help layout stays byte-stable.
    JsonCompact,
    Text,
}

impl From<CliOutputMode> for OutputMode {
    fn from(mode: CliOutputMode) -> Self {
        match mode {
            CliOutputMode::Json => OutputMode::Json,
            CliOutputMode::JsonCompact => OutputMode::JsonCompact,
            CliOutputMode::Text => OutputMode::Text,
        }
    }
}
