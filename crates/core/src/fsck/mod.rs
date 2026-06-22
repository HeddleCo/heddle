// SPDX-License-Identifier: Apache-2.0
//! Repository integrity checks.

mod bridge;
mod objects;
mod refs;
mod repair;
mod state;
#[cfg(test)]
mod tests;

use ::objects::{HeddleError, error::Result};
use serde::Serialize;

use crate::ExecutionContext;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsckOptions {
    pub full: bool,
    pub thorough: bool,
    pub repair: bool,
    pub bridge: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FsckReport {
    pub valid: bool,
    pub errors: Vec<FsckError>,
    pub warnings: Vec<String>,
    pub objects_checked: usize,
    pub bridge_checked: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
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
        objects::check_trees(repo, &mut errors, &mut warnings, &mut objects_checked)?;
        objects::check_blobs(repo, &mut errors, &mut warnings, &mut objects_checked)?;
    }

    refs::check_refs(repo, &mut errors, &mut warnings)?;
    refs::check_merge_state(repo, &mut warnings)?;
    if opts.bridge {
        bridge::check_bridge(repo, &mut errors, &mut warnings, &mut objects_checked)?;
    }

    let valid = errors.is_empty();

    if opts.repair && !valid {
        repair::repair_issues(repo, &errors)?;
    }

    Ok(FsckReport {
        valid,
        errors,
        warnings,
        objects_checked,
        bridge_checked: opts.bridge,
    })
}

fn invalid_fsck_config(message: impl Into<String>) -> HeddleError {
    HeddleError::Config(message.into())
}
