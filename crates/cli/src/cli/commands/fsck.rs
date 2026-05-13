// SPDX-License-Identifier: Apache-2.0
//! Fsck command - verify repository integrity.

use anyhow::{Result, anyhow};
use repo::Repository;
use serde::Serialize;

use super::fsck_checks::{
    FsckError, check_blobs, check_merge_state, check_refs, check_states, check_trees, make_error,
    repair_issues,
};
use crate::{
    bridge::{GitBridge, git_notes},
    cli::{Cli, should_output_json, style},
};

#[derive(Serialize)]
struct FsckOutput {
    valid: bool,
    errors: Vec<FsckError>,
    warnings: Vec<String>,
    objects_checked: usize,
    bridge_checked: bool,
}

pub fn cmd_fsck(cli: &Cli, full: bool, thorough: bool, repair: bool, bridge: bool) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    let mut errors: Vec<FsckError> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut objects_checked: usize = 0;

    check_states(&repo, &mut errors, &mut objects_checked, thorough)?;

    if full {
        check_trees(&repo, &mut errors, &mut warnings, &mut objects_checked)?;
        check_blobs(&repo, &mut errors, &mut warnings, &mut objects_checked)?;
    }

    check_refs(&repo, &mut errors, &mut warnings)?;
    check_merge_state(&repo, &mut warnings)?;
    if bridge {
        check_bridge(&repo, &mut errors, &mut warnings, &mut objects_checked)?;
    }

    let valid = errors.is_empty();

    if repair && !valid {
        repair_issues(&repo, &errors)?;
    }

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&FsckOutput {
                valid,
                errors,
                warnings,
                objects_checked,
                bridge_checked: bridge,
            })?
        );
    } else {
        if valid {
            let counted = style::count(objects_checked, "object");
            println!(
                "{} repository is valid ({counted} checked)",
                style::ok_marker(),
            );
            if bridge {
                println!("  {}", style::field("Bridge", "mirror and mapping checked"));
            }
        } else {
            println!(
                "{} repository has {}",
                style::error_marker(),
                style::count(errors.len(), "integrity error")
            );
            for error in &errors {
                if let Some(obj) = &error.object {
                    println!(
                        "  {} {} {}",
                        style::error(&format!("[{}]", error.kind)),
                        error.message,
                        style::dim(&format!("({obj})"))
                    );
                } else {
                    println!(
                        "  {} {}",
                        style::error(&format!("[{}]", error.kind)),
                        error.message
                    );
                }
            }
        }
        for warning in &warnings {
            println!("{} {}", style::warn_marker(), warning);
        }
    }

    if !valid {
        return Err(anyhow!("Repository has integrity errors"));
    }

    Ok(())
}

fn check_bridge(
    repo: &Repository,
    errors: &mut Vec<FsckError>,
    warnings: &mut Vec<String>,
    objects_checked: &mut usize,
) -> Result<()> {
    let mut bridge = GitBridge::new(repo);
    if !bridge.is_initialized() {
        warnings.push("Git-overlay mirror has not been initialized yet".to_string());
        return Ok(());
    }

    bridge
        .build_existing_mapping(None)
        .map_err(|err| anyhow!("bridge mapping check failed: {err}"))?;
    let mirror = bridge
        .open_git_repo()
        .map_err(|err| anyhow!("bridge mirror open failed: {err}"))?;

    for (change_id, git_oid) in bridge.mapping.iter() {
        *objects_checked += 1;
        if mirror.find_object(*git_oid).is_err() {
            errors.push(make_error(
                "bridge-mapping",
                &format!("mapped Git object {git_oid} is missing from the mirror"),
                Some(change_id.to_string()),
            ));
        }
        if repo.store().get_state(change_id)?.is_none() {
            errors.push(make_error(
                "bridge-mapping",
                &format!("mapped Heddle state {change_id} is missing from the store"),
                Some(git_oid.to_string()),
            ));
        }
    }

    for (git_oid, note) in git_notes::read_all_notes(&mirror)
        .map_err(|err| anyhow!("bridge notes check failed: {err}"))?
    {
        *objects_checked += 1;
        let Ok(change_id) = objects::object::ChangeId::parse(&note.change_id) else {
            errors.push(make_error(
                "bridge-notes",
                &format!("note for {git_oid} contains an invalid Heddle change id"),
                Some(note.change_id),
            ));
            continue;
        };
        if bridge.mapping.get_git(&change_id) != Some(git_oid) {
            errors.push(make_error(
                "bridge-notes",
                &format!("note for {git_oid} does not round-trip through the bridge mapping"),
                Some(change_id.to_string()),
            ));
        }
    }

    for thread in repo.refs().list_threads()? {
        let Some(state_id) = repo.refs().get_thread(&thread)? else {
            continue;
        };
        *objects_checked += 1;
        if repo.store().get_state(&state_id)?.is_none() {
            errors.push(make_error(
                "bridge-thread",
                &format!("thread '{thread}' points at a missing state"),
                Some(state_id.to_string()),
            ));
        }
    }

    check_checkout_head(repo, &bridge, errors, objects_checked)?;
    Ok(())
}

fn check_checkout_head(
    repo: &Repository,
    bridge: &GitBridge<'_>,
    errors: &mut Vec<FsckError>,
    objects_checked: &mut usize,
) -> Result<()> {
    let Ok(checkout) = gix::discover(repo.root()) else {
        return Ok(());
    };
    let refs::Head::Attached { thread } = repo.head_ref()? else {
        return Ok(());
    };
    let Some(state_id) = repo.refs().get_thread(&thread)? else {
        return Ok(());
    };
    let Some(expected_git_oid) = bridge.mapping.get_git(&state_id) else {
        return Ok(());
    };
    let branch_ref = format!("refs/heads/{thread}");
    let Ok(mut reference) = checkout.find_reference(&branch_ref) else {
        return Ok(());
    };
    let actual_git_oid = reference
        .peel_to_id()
        .map_err(|err| anyhow!("checkout HEAD check failed: {err}"))?
        .detach();
    *objects_checked += 1;
    if actual_git_oid != expected_git_oid {
        errors.push(make_error(
            "bridge-checkout",
            &format!(
                "checkout branch '{thread}' points at {actual_git_oid}, but Heddle maps the attached thread to {expected_git_oid}"
            ),
            Some(state_id.to_string()),
        ));
    }
    Ok(())
}