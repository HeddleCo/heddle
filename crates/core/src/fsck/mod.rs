// SPDX-License-Identifier: Apache-2.0
//! Repository integrity checks.

mod bridge;
mod objects;
mod refs;
mod state;
#[cfg(test)]
mod tests;

use ::objects::{HeddleError, error::Result};
use schemars::JsonSchema;
use serde::Serialize;

use crate::{ExecutionContext, HeddleReport, MachineOutputKind, ReportContract, schema_for_report};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsckOptions {
    pub full: bool,
    pub thorough: bool,
    pub bridge: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct FsckReport {
    pub valid: bool,
    pub errors: Vec<FsckError>,
    pub warnings: Vec<String>,
    pub objects_checked: usize,
    pub bridge_checked: bool,
    pub repair_target: Option<String>,
    pub repaired: bool,
    pub repairs: Vec<FsckRepair>,
}

impl FsckReport {
    pub const CONTRACT: ReportContract = ReportContract {
        schema_name: "fsck",
        machine_output_kind: MachineOutputKind::Json,
        output_discriminator: None,
        schema: schema_for_report::<FsckReport>,
    };
}

impl HeddleReport for FsckReport {
    const CONTRACT: ReportContract = FsckReport::CONTRACT;
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct FsckRepair {
    pub name: String,
    pub repaired: bool,
    pub detail: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct FsckError {
    pub kind: String,
    pub message: String,
    pub object: Option<String>,
}

fn make_error(kind: &str, message: &str, object: Option<String>) -> FsckError {
    FsckError {
        kind: kind.to_string(),
        message: message.to_string(),
        object,
    }
}

pub fn fsck(ctx: &ExecutionContext, opts: FsckOptions) -> Result<FsckReport> {
    let repo = ctx.require_repo()?;

    let mut errors: Vec<FsckError> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut objects_checked: usize = 0;

    state::check_states(repo, &mut errors, &mut objects_checked, opts.thorough)?;

    if opts.full {
        objects::check_tree_objects(repo, &mut errors, &mut warnings, &mut objects_checked)?;
    }

    refs::check_refs(repo, &mut errors, &mut warnings)?;
    refs::check_merge_state(repo, &mut warnings)?;
    if opts.bridge {
        bridge::check_bridge(repo, &mut errors, &mut warnings, &mut objects_checked)?;
    }

    let valid = errors.is_empty();

    Ok(FsckReport {
        valid,
        errors,
        warnings,
        objects_checked,
        bridge_checked: opts.bridge,
        repair_target: None,
        repaired: false,
        repairs: Vec::new(),
    })
}

fn invalid_fsck_config(message: impl Into<String>) -> HeddleError {
    HeddleError::Config(message.into())
}
