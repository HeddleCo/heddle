// SPDX-License-Identifier: Apache-2.0
//! Repository integrity checks.

mod git_projection;
mod objects;
mod provenance;
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
    pub provenance: bool,
    pub git_projection: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct FsckReport {
    pub valid: bool,
    pub errors: Vec<FsckError>,
    pub warnings: Vec<String>,
    pub objects_checked: usize,
    pub git_projection_checked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<crate::ProvenanceReport>,
    pub repair_target: Option<String>,
    pub repaired: bool,
    pub repairs: Vec<FsckRepair>,
}

impl FsckReport {
    pub const CONTRACT: ReportContract = ReportContract {
        schema_name: "maintenance fsck",
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

    let provenance = (opts.thorough && opts.provenance)
        .then(|| crate::verify_repository_provenance(repo))
        .transpose()?;
    if let Some(provenance) = &provenance {
        for state in &provenance.states {
            match (state.status.as_str(), state.failed_link.as_deref()) {
                ("Legacy", _) => errors.push(make_error(
                    "legacy_provenance",
                    &format!(
                        "State {} Legacy at content link: {}",
                        state.state_id, state.detail
                    ),
                    Some(state.state_id.clone()),
                )),
                (_, Some(_)) => errors.push(make_error(
                    "invalid_provenance_chain",
                    &format!(
                        "State {} {}: {}",
                        state.state_id,
                        state.display_status(),
                        state.detail
                    ),
                    Some(state.state_id.clone()),
                )),
                _ => {}
            }
        }
    }

    if opts.full {
        objects::check_tree_objects(repo, &mut errors, &mut warnings, &mut objects_checked)?;
    }

    refs::check_refs(repo, &mut errors, &mut warnings)?;
    refs::check_merge_state(repo, &mut warnings)?;
    if opts.git_projection {
        git_projection::check_git_projection(
            repo,
            &mut errors,
            &mut warnings,
            &mut objects_checked,
        )?;
    }

    let valid = errors.is_empty();

    Ok(FsckReport {
        valid,
        errors,
        warnings,
        objects_checked,
        git_projection_checked: opts.git_projection,
        provenance,
        repair_target: None,
        repaired: false,
        repairs: Vec::new(),
    })
}

fn invalid_fsck_config(message: impl Into<String>) -> HeddleError {
    HeddleError::Config(message.into())
}
