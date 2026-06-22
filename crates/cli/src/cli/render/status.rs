// SPDX-License-Identifier: Apache-2.0
//! Renderers for `heddle status`.

use anyhow::Result;

use crate::cli::{
    Cli,
    commands::status::{self, StatusOutput},
};

pub(crate) fn status_text(cli: &Cli, report: &StatusOutput, short: bool) -> Result<()> {
    status::render_status(cli, report, short)
}

pub(crate) fn status_json(cli: &Cli, report: &StatusOutput) -> Result<()> {
    status::render_status(cli, report, false)
}
