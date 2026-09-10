// SPDX-License-Identifier: Apache-2.0
//! `heddle grant` — create, list, and delete spool collaborator grants.

use anyhow::{Context, Result, anyhow};
use heddle_cli_contract::cli::commands::wire::auth::{
    GrantCreateOutput, GrantDeleteOutput, GrantListOutput, GrantRowOutput,
};
use hosted_client::hosted_runtime::{
    auth::resolve_server,
    hosted::{HostedAuthMode, HostedClient, HostedSession, canonicalize_spool_path},
};
use wire::{HostedGrantInfo, ProtocolError};

use super::{
    advice::RecoveryAdvice,
    next_action::{NextActionValidationContext, write_full_command_json},
};
use crate::{
    cli::{
        Cli, GrantCommands, GrantCreateArgs, GrantDeleteArgs, GrantListArgs, should_output_json,
        style,
    },
    config::UserConfig,
    remote::RemoteTarget,
};

pub async fn cmd_grant(cli: &Cli, command: GrantCommands) -> Result<()> {
    match command {
        GrantCommands::Create(args) => cmd_grant_create(cli, args).await,
        GrantCommands::List(args) => cmd_grant_list(cli, args).await,
        GrantCommands::Delete(args) => cmd_grant_delete(cli, args).await,
    }
}

async fn cmd_grant_create(cli: &Cli, args: GrantCreateArgs) -> Result<()> {
    let (server, spool) = resolve_grant_spool(&args.spool, args.server.as_deref())?;
    let mut session = hosted_connect(&server).await?;
    let result = create_connected(
        cli,
        &mut session,
        &server,
        &spool,
        &args.principal,
        args.role.as_hosted_role_name(),
    )
    .await;
    session.close().await;
    result
}

async fn cmd_grant_list(cli: &Cli, args: GrantListArgs) -> Result<()> {
    let (server, spool) = resolve_grant_spool(&args.spool, args.server.as_deref())?;
    let mut session = hosted_connect(&server).await?;
    let result = list_connected(cli, &mut session, &server, &spool).await;
    session.close().await;
    result
}

async fn cmd_grant_delete(cli: &Cli, args: GrantDeleteArgs) -> Result<()> {
    let (server, spool) = resolve_grant_spool(&args.spool, args.server.as_deref())?;
    let mut session = hosted_connect(&server).await?;
    let result = delete_connected(cli, &mut session, &server, &spool, &args.id).await;
    session.close().await;
    result
}

async fn hosted_connect(server: &str) -> Result<HostedClient> {
    let user_config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &user_config,
        Some(server.to_string()),
        HostedAuthMode::CredentialFallback,
    )?;
    session
        .connect(server)
        .await
        .map_err(|err| map_grant_error("", &err))
}

async fn create_connected(
    cli: &Cli,
    client: &mut HostedClient,
    server: &str,
    spool: &str,
    principal: &str,
    role: &str,
) -> Result<()> {
    let created = client
        .create_grant(principal, role, None, Some(spool), cli.operation_id_wire())
        .await
        .map_err(|err| map_grant_error(spool, &err))?;
    let row = grant_row(&created, spool);
    if should_output_json(cli, None) {
        write_full_command_json(
            &GrantCreateOutput {
                output_kind: "grant_create",
                id: row.id.clone(),
                principal: row.principal.clone(),
                role: row.role.clone(),
                spool: row.spool.clone(),
                server: server.to_string(),
                recommended_action: Some(format!("heddle grant list --spool {spool}")),
            },
            NextActionValidationContext::without_repo(&["grant", "create"]),
        )?;
    } else {
        println!(
            "{} granted {} {} on {}",
            style::ok_marker(),
            style::bold(&row.principal),
            style::bold(&row.role),
            style::bold(&row.spool)
        );
        super::action_line::print_next(&format!("heddle grant list --spool {spool}"));
    }
    Ok(())
}

async fn list_connected(
    cli: &Cli,
    client: &mut HostedClient,
    server: &str,
    spool: &str,
) -> Result<()> {
    let grants = client
        .list_grants(Some(&format!("repo:{spool}")))
        .await
        .map_err(|err| map_grant_error(spool, &err))?;
    let rows: Vec<GrantRowOutput> = grants.iter().map(|grant| grant_row(grant, spool)).collect();
    if should_output_json(cli, None) {
        write_full_command_json(
            &GrantListOutput {
                output_kind: "grant_list",
                spool: spool.to_string(),
                server: server.to_string(),
                grants: rows,
                recommended_action: None,
            },
            NextActionValidationContext::without_repo(&["grant", "list"]),
        )?;
    } else if rows.is_empty() {
        println!("No grants on {}.", style::bold(spool));
        super::action_line::print_next(&format!(
            "heddle grant create --spool {spool} --principal <handle> --role contributor"
        ));
    } else {
        println!("ID\tROLE\tSPOOL");
        for row in rows {
            println!("{}\t{}\t{}", row.id, row.role, row.spool);
        }
    }
    Ok(())
}

async fn delete_connected(
    cli: &Cli,
    client: &mut HostedClient,
    server: &str,
    spool: &str,
    principal: &str,
) -> Result<()> {
    client
        .delete_grant(principal, None, Some(spool), cli.operation_id_wire())
        .await
        .map_err(|err| map_grant_error(spool, &err))?;
    if should_output_json(cli, None) {
        write_full_command_json(
            &GrantDeleteOutput {
                output_kind: "grant_delete",
                id: principal.to_string(),
                principal: principal.to_string(),
                spool: spool.to_string(),
                server: server.to_string(),
                deleted: true,
                recommended_action: Some(format!("heddle grant list --spool {spool}")),
            },
            NextActionValidationContext::without_repo(&["grant", "delete"]),
        )?;
    } else {
        println!(
            "{} removed {} from {}",
            style::ok_marker(),
            style::bold(principal),
            style::bold(spool)
        );
        super::action_line::print_next(&format!("heddle grant list --spool {spool}"));
    }
    Ok(())
}

fn grant_row(grant: &HostedGrantInfo, fallback_spool: &str) -> GrantRowOutput {
    let spool = grant
        .repo_path
        .as_deref()
        .or(grant.namespace_path.as_deref())
        .unwrap_or(fallback_spool)
        .to_string();
    GrantRowOutput {
        id: grant.subject.clone(),
        principal: grant.subject.clone(),
        role: grant.role.clone(),
        spool,
    }
}

fn resolve_grant_spool(spool: &str, server: Option<&str>) -> Result<(String, String)> {
    if spool.starts_with("https://") {
        match RemoteTarget::parse(spool) {
            Ok(RemoteTarget::Network {
                authority,
                repo_path: Some(repo_path),
            }) => {
                let full_path = canonicalize_spool_path(&repo_path).map_err(anyhow::Error::new)?;
                return Ok((authority, full_path));
            }
            Ok(_) | Err(_) => {
                return Err(anyhow!(
                    "hosted grant URL must include a spool path, e.g. https://api.heddle.sh/spool/<handle>/<name>"
                ));
            }
        }
    }
    let server = resolve_server(server).context("resolve hosted server for grant")?;
    let full_path = canonicalize_spool_path(spool).map_err(anyhow::Error::new)?;
    Ok((server, full_path))
}

fn map_grant_error(spool: &str, err: &ProtocolError) -> anyhow::Error {
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
    let advice = if matches!(
        err,
        ProtocolError::AuthorizationFailed(_) | ProtocolError::AuthenticationFailed(_)
    ) || lower.contains("permission")
        || lower.contains("not authorized")
        || lower.contains("forbidden")
    {
        RecoveryAdvice::grant_denied(spool)
    } else if matches!(err, ProtocolError::ObjectNotFound(_)) || lower.contains("not found") {
        RecoveryAdvice::grant_not_found(spool)
    } else {
        let display = if message.is_empty() {
            err.to_string()
        } else {
            message.to_string()
        };
        RecoveryAdvice::grant_failed(spool, &display)
    };
    anyhow!(advice)
}

#[cfg(test)]
mod tests {
    use wire::ProtocolError;

    use super::{grant_row, map_grant_error, resolve_grant_spool};

    #[test]
    fn url_target_takes_host_and_canonical_path() {
        let (server, path) = resolve_grant_spool(
            "https://api.preview.heddle.sh/willow-ibis-8e7264/notes",
            None,
        )
        .expect("parse url");
        assert_eq!(server, "api.preview.heddle.sh");
        assert_eq!(path, "spool/willow-ibis-8e7264/notes");
    }

    #[test]
    fn canonical_spool_path_is_not_a_hostname() {
        let (server, path) =
            resolve_grant_spool("spool/pine-yak-87fa33/repo", Some("api.preview.heddle.sh"))
                .expect("canonical path");
        assert_eq!(server, "api.preview.heddle.sh");
        assert_eq!(path, "spool/pine-yak-87fa33/repo");
    }

    #[test]
    fn grant_row_uses_subject_as_delete_id() {
        let row = grant_row(
            &wire::HostedGrantInfo {
                subject: "alice".into(),
                role: "developer".into(),
                namespace_path: None,
                repo_path: Some("spool/me/notes".into()),
            },
            "fallback",
        );
        assert_eq!(row.id, "alice");
        assert_eq!(row.principal, "alice");
        assert_eq!(row.role, "developer");
        assert_eq!(row.spool, "spool/me/notes");
    }

    #[test]
    fn permission_denial_is_not_swallowed() {
        let err = ProtocolError::AuthorizationFailed("missing grant:write".into());
        let mapped = map_grant_error("spool/alice/notes", &err);
        let advice = mapped
            .downcast_ref::<crate::cli::commands::RecoveryAdvice>()
            .expect("advice");
        assert_eq!(advice.kind, "grant_denied");
    }

    #[test]
    fn missing_spool_is_not_swallowed() {
        let err = ProtocolError::ObjectNotFound("unknown spool".into());
        let mapped = map_grant_error("spool/alice/notes", &err);
        let advice = mapped
            .downcast_ref::<crate::cli::commands::RecoveryAdvice>()
            .expect("advice");
        assert_eq!(advice.kind, "grant_not_found");
    }
}
