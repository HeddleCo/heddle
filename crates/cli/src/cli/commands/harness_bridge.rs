// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;

use crate::{cli::Cli, harness};

pub fn cmd_harness_bridge(cli: &Cli) -> Result<()> {
    harness::cmd_harness_bridge(cli)
}