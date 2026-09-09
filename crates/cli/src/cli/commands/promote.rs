// SPDX-License-Identifier: Apache-2.0
//! `heddle promote` — lift a personal hosted spool to the shared root.

use anyhow::{Context, Result, anyhow};
use heddle_cli_contract::cli::commands::wire::auth::PromoteOutput;
use hosted_client::hosted_runtime::{
    auth::resolve_server,
    hosted::{
        HostedAuthMode, HostedClient, HostedSession, canonicalize_spool_path,
        is_root_level_spool_path,
    },
};
use wire::ProtocolError;

use super::{
    advice::RecoveryAdvice,
    next_action::{NextActionValidationContext, write_full_command_json},
};
use crate::{
    cli::{Cli, CliContext, PromoteArgs, should_output_json, style},
    config::UserConfig,
    remote::RemoteTarget,
};

pub async fn cmd_promote(cli: &Cli, args: PromoteArgs) -> Result<()> {
    let (server, typed_path) = split_promote_target(&args.path, args.server.as_deref())?;
    let full_path = canonicalize_spool_path(&typed_path).map_err(anyhow::Error::new)?;
    if is_root_level_spool_path(&full_path) {
        return Err(anyhow!(RecoveryAdvice::promote_already_root(&full_path)));
    }

    let user_config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &user_config,
        Some(server.clone()),
        HostedAuthMode::CredentialFallback,
    )?;
    let mut client = session.connect(&server).await?;
    let result = promote_connected(cli, &mut client, &server, &full_path).await;
    client.close().await;
    result
}

async fn promote_connected(
    cli: &Cli,
    client: &mut HostedClient,
    server: &str,
    full_path: &str,
) -> Result<()> {
    let promoted = client
        .promote_spool(full_path, &cli.operation_id_wire())
        .await
        .map_err(|err| map_promote_error(full_path, &err))?;
    if should_output_json(cli, None) {
        write_full_command_json(
            &PromoteOutput {
                output_kind: "promote",
                status: "promoted",
                from: full_path.to_string(),
                full_path: promoted.full_path.clone(),
                spool_id: promoted.spool_id,
                is_repo: promoted.is_repo,
                server: server.to_string(),
                recommended_action: None,
            },
            NextActionValidationContext::without_repo(&["promote"]),
        )?;
    } else {
        println!(
            "{} promoted {} → {}",
            style::ok_marker(),
            style::bold(full_path),
            style::bold(&promoted.full_path)
        );
        super::action_line::print_next(&format!(
            "heddle clone https://{server}/{} <dir>",
            promoted.full_path
        ));
    }
    Ok(())
}

fn split_promote_target(path: &str, server: Option<&str>) -> Result<(String, String)> {
    // Only `https://host/...` is a remote URL. A canonical spool path such as
    // `spool/<handle>/<name>` must not be parsed as host `spool`.
    if path.starts_with("https://") {
        match RemoteTarget::parse(path) {
            Ok(RemoteTarget::Network {
                authority,
                repo_path: Some(repo_path),
            }) => return Ok((authority, repo_path)),
            Ok(_) | Err(_) => {
                return Err(anyhow!(
                    "hosted promote URL must include a spool path, e.g. https://api.heddle.sh/spool/<handle>/<name>"
                ));
            }
        }
    }
    let server = resolve_server(server).context("resolve hosted server for promote")?;
    Ok((server, path.to_string()))
}

fn map_promote_error(full_path: &str, err: &ProtocolError) -> anyhow::Error {
    let slug = full_path.rsplit('/').next().unwrap_or(full_path);
    let message = match err {
        ProtocolError::RemoteFailure { message, .. } => message.as_str(),
        ProtocolError::AlreadyExists(message)
        | ProtocolError::AuthorizationFailed(message)
        | ProtocolError::AuthenticationFailed(message)
        | ProtocolError::InvalidState(message)
        | ProtocolError::ObjectNotFound(message)
        | ProtocolError::Remote(message) => message.as_str(),
        _ => "",
    };
    let lower = message.to_ascii_lowercase();
    let advice = if matches!(err, ProtocolError::AlreadyExists(_))
        || lower.contains("already exists")
        || lower.contains("taken")
        || lower.contains("reserved")
    {
        RecoveryAdvice::promote_slug_taken(full_path, slug)
    } else if lower.contains("claim")
        || lower.contains("verif")
        || lower.contains("standing")
        || lower.contains("anonymous")
        || lower.contains("unverified")
    {
        RecoveryAdvice::promote_account_standing(full_path)
    } else if matches!(
        err,
        ProtocolError::AuthorizationFailed(_) | ProtocolError::AuthenticationFailed(_)
    ) || lower.contains("owner")
        || lower.contains("permission")
    {
        RecoveryAdvice::promote_not_owner(full_path)
    } else {
        let display = if message.is_empty() {
            err.to_string()
        } else {
            message.to_string()
        };
        RecoveryAdvice::promote_failed(full_path, &display)
    };
    anyhow!(advice)
}

#[cfg(test)]
mod tests {
    use hosted_client::hosted_runtime::hosted::{
        canonicalize_spool_path, is_root_level_spool_path,
    };
    use wire::ProtocolError;

    use super::{map_promote_error, split_promote_target};

    #[test]
    fn url_target_takes_host_and_path() {
        let (server, path) = split_promote_target(
            "https://api.preview.heddle.sh/willow-ibis-8e7264/notes",
            None,
        )
        .expect("parse url");
        assert_eq!(server, "api.preview.heddle.sh");
        assert_eq!(path, "willow-ibis-8e7264/notes");
    }

    #[test]
    fn canonical_spool_path_is_not_a_hostname() {
        let (server, path) =
            split_promote_target("spool/pine-yak-87fa33/repo", Some("api.preview.heddle.sh"))
                .expect("canonical path");
        assert_eq!(server, "api.preview.heddle.sh");
        assert_eq!(path, "spool/pine-yak-87fa33/repo");
    }

    #[test]
    fn canonical_personal_path_is_not_root() {
        let path = canonicalize_spool_path("willow-ibis-8e7264/notes").expect("path");
        assert_eq!(path, "spool/willow-ibis-8e7264/notes");
        assert!(!is_root_level_spool_path(&path));
    }

    #[test]
    fn slug_taken_denial_is_not_swallowed() {
        let err = ProtocolError::AlreadyExists("root slug notes is taken".into());
        let mapped = map_promote_error("spool/alice/notes", &err);
        let advice = mapped
            .downcast_ref::<crate::cli::commands::RecoveryAdvice>()
            .expect("advice");
        assert_eq!(advice.kind, "promote_slug_taken");
        assert!(advice.error.contains("notes"));
    }

    #[test]
    fn standing_denial_is_not_swallowed() {
        let err = ProtocolError::InvalidState("account is not claimed/verified".into());
        let mapped = map_promote_error("spool/alice/notes", &err);
        let advice = mapped
            .downcast_ref::<crate::cli::commands::RecoveryAdvice>()
            .expect("advice");
        assert_eq!(advice.kind, "promote_account_standing");
        assert!(advice.hint.contains("heddle claim"));
    }

    #[test]
    fn owner_denial_is_not_swallowed() {
        let err = ProtocolError::AuthorizationFailed("missing owner grant".into());
        let mapped = map_promote_error("spool/alice/notes", &err);
        let advice = mapped
            .downcast_ref::<crate::cli::commands::RecoveryAdvice>()
            .expect("advice");
        assert_eq!(advice.kind, "promote_not_owner");
    }
}
