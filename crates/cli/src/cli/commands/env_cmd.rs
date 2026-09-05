// SPDX-License-Identifier: Apache-2.0
//! `heddle env` — broker-backed confidential-runtime profiles.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use crypto::Signer;
use repo::Repository;
use runtime_profile::{DecryptPurpose, DecryptRequest, PolicyBroker, RuntimeProfileStore, SlotWrite};
use serde::Serialize;

use super::advice::RecoveryAdvice;
use super::next_action::{NextActionValidationContext, write_full_command_json};
use crate::cli::{
    Cli, EnvCommands, EnvCreateArgs, EnvListArgs, EnvRunArgs, should_output_json,
};

pub fn cmd_env(cli: &Cli, command: EnvCommands) -> Result<()> {
    let repo = cli.open_repo()?;
    match command {
        EnvCommands::Create(args) => cmd_env_create(cli, &repo, args),
        EnvCommands::List(_) => cmd_env_list(cli, &repo, EnvListArgs {}),
        EnvCommands::Run(args) => cmd_env_run(&repo, args),
    }
}

#[derive(Serialize)]
struct EnvCreateOutput {
    output_kind: &'static str,
    profile: String,
    slots: Vec<String>,
}

#[derive(Serialize)]
struct EnvListOutput {
    output_kind: &'static str,
    profiles: Vec<EnvProfileRow>,
}

#[derive(Serialize)]
struct EnvProfileRow {
    name: String,
    slots: Vec<String>,
    lifecycle: String,
}

fn cmd_env_create(cli: &Cli, repo: &Repository, args: EnvCreateArgs) -> Result<()> {
    let store = RuntimeProfileStore::open(repo.heddle_dir()).map_err(map_profile_error)?;
    let signer = require_signer(repo)?;
    let attribution = repo
        .get_attribution()
        .context("resolve current attribution")?;
    let recipient = store
        .default_or_create_software_recipient(&signer)
        .map_err(map_profile_error)?;
    let mut slots = Vec::new();
    for name in &args.from_env {
        let value = std::env::var(name).map_err(|_| {
            anyhow!(RecoveryAdvice::safety_refusal(
                "runtime_profile_slot_not_found",
                format!("environment variable {name} is not set"),
                format!("Export `{name}` in this process, then retry `heddle env create`."),
                format!("{name} was not present in the creating process"),
                "no runtime profile would be written",
                "the worktree and existing profiles were left unchanged",
                "heddle env list",
                vec!["heddle env list".to_string()],
            ))
        })?;
        slots.push(SlotWrite {
            name: name.clone(),
            value: value.into_bytes(),
        });
    }
    let slot_names: Vec<String> = slots.iter().map(|slot| slot.name.clone()).collect();
    let profile = store
        .create_profile(
            &args.name,
            slots,
            recipient.recipient_id,
            attribution,
            &signer,
        )
        .map_err(map_profile_error)?;
    if should_output_json(cli, Some(repo.config())) {
        return write_full_command_json(
            &EnvCreateOutput {
                output_kind: "env_create",
                profile: profile.name,
                slots: slot_names,
            },
            NextActionValidationContext::new(&["env", "create"], repo.capability()),
        );
    }
    eprintln!(
        "created {} ({})",
        profile.name,
        if slot_names.is_empty() {
            "no slots".to_string()
        } else {
            slot_names.join(" ")
        }
    );
    Ok(())
}

fn cmd_env_list(cli: &Cli, repo: &Repository, _args: EnvListArgs) -> Result<()> {
    let store = RuntimeProfileStore::open(repo.heddle_dir()).map_err(map_profile_error)?;
    let profiles = store.list_profiles().map_err(map_profile_error)?;
    if should_output_json(cli, Some(repo.config())) {
        return write_full_command_json(
            &EnvListOutput {
                output_kind: "env_list",
                profiles: profiles
                    .iter()
                    .map(|profile| EnvProfileRow {
                        name: profile.name.clone(),
                        slots: profile.slot_names.clone(),
                        lifecycle: profile.lifecycle.to_string(),
                    })
                    .collect(),
            },
            NextActionValidationContext::new(&["env", "list"], repo.capability()),
        );
    }
    if profiles.is_empty() {
        eprintln!("no runtime profiles");
        return Ok(());
    }
    for profile in profiles {
        eprintln!(
            "{}  {}  ({})",
            profile.name,
            profile.slot_names.join(" "),
            profile.lifecycle
        );
    }
    Ok(())
}

fn cmd_env_run(repo: &Repository, args: EnvRunArgs) -> Result<()> {
    if args.command.is_empty() {
        return Err(anyhow!("env run requires a child command after `--`"));
    }
    let store = RuntimeProfileStore::open(repo.heddle_dir()).map_err(map_profile_error)?;
    let signer = require_signer(repo)?;
    let attribution = repo
        .get_attribution()
        .context("resolve current attribution")?;
    let mut broker = PolicyBroker::new(store, attribution);
    broker
        .hold_profile_recipients(&args.profile)
        .map_err(map_profile_error)?;
    let now = now_ms()?;
    let ttl_ms = i64::try_from(args.ttl.saturating_mul(1000))
        .map_err(|_| anyhow!("ttl overflow"))?;
    let request = DecryptRequest {
        profile: args.profile.clone(),
        slots: args.slots.clone(),
        expires_at_ms: now.saturating_add(ttl_ms),
        purpose: DecryptPurpose::Run,
        caller: "heddle-env-run".to_string(),
    };
    let grant = broker
        .authorize(&request, &signer)
        .map_err(map_profile_error)?;
    let secrets = broker
        .unwrap_for_run(grant, &signer)
        .map_err(map_profile_error)?;
    let pairs = secrets.into_env_pairs().map_err(map_profile_error)?;
    let mut child = Command::new(&args.command[0]);
    child.args(&args.command[1..]);
    child.current_dir(repo.root());
    for (name, value) in pairs {
        child.env(name, value);
    }
    let status = child.status().context("spawn env run child")?;
    match status.code() {
        Some(0) => Ok(()),
        Some(code) => std::process::exit(code),
        None => Err(anyhow!("child was terminated by a signal")),
    }
}

fn require_signer(repo: &Repository) -> Result<Box<dyn Signer>> {
    let local = repo.heddle_dir().join(repo::identity::LOCAL_IDENTITY_FILE);
    let device = repo::identity::device_identity_path();
    repo::identity::resolve_signer(&local, &device).ok_or_else(|| {
        anyhow!("runtime profiles require a protected local signing identity")
    })
}

fn now_ms() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before epoch")?;
    i64::try_from(duration.as_millis()).context("timestamp overflow")
}

fn map_profile_error(err: runtime_profile::RuntimeProfileError) -> anyhow::Error {
    let message = err.to_string();
    match err {
        runtime_profile::RuntimeProfileError::BrokerDenied(
            runtime_profile::BrokerDenialReason::Expired,
        ) => {
            anyhow!(RecoveryAdvice::safety_refusal(
                "runtime_profile_expired",
                message,
                "Retry `heddle env run --profile <name> -- <cmd>`. Use a larger --ttl only if authorize itself is slow.",
                "the broker request or grant was past expires_at",
                "no child would start and no plaintext would be written",
                "the worktree and store were left unchanged",
                "heddle env run --profile <name> -- <cmd>",
                vec!["heddle env run --profile <name> -- <cmd>".to_string()],
            ))
        }
        runtime_profile::RuntimeProfileError::BrokerDenied(_)
        | runtime_profile::RuntimeProfileError::InvalidGrant(_) => anyhow!(
            RecoveryAdvice::safety_refusal(
                "runtime_profile_denied",
                message,
                "Ask for a named profile and slots this broker holds, then retry `heddle env run`.",
                "the broker refused an unauthorized, expired, or unscoped request",
                "no plaintext would be returned",
                "the worktree and store were left unchanged",
                "heddle env list",
                vec!["heddle env list".to_string()],
            )
        ),
        runtime_profile::RuntimeProfileError::ProfileNotFound(_) => anyhow!(
            RecoveryAdvice::safety_refusal(
                "runtime_profile_not_found",
                message,
                "Create the profile with `heddle env create --name <name> --from-env SLOT`, then retry.",
                "no runtime profile with that name exists",
                "no child would start",
                "the worktree and store were left unchanged",
                "heddle env create --name <name> --from-env SLOT",
                vec!["heddle env create --name <name> --from-env SLOT".to_string()],
            )
        ),
        runtime_profile::RuntimeProfileError::SlotNotFound(_) => anyhow!(
            RecoveryAdvice::safety_refusal(
                "runtime_profile_slot_not_found",
                message,
                "List slot names with `heddle env list`, then pass a slot that exists.",
                "the named slot is not on the profile head",
                "no child would start",
                "the worktree and store were left unchanged",
                "heddle env list",
                vec!["heddle env list".to_string()],
            )
        ),
        other => anyhow!(other),
    }
}
