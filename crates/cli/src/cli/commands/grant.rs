// SPDX-License-Identifier: Apache-2.0
//! `heddle grant` — create, list, and delete spool collaborator grants.

use anyhow::{Context, Result, anyhow};
use api::heddle::api::v2alpha1::{GrantRecord, ResourceRole};
use heddle_cli_contract::cli::commands::wire::auth::{
    GrantCreateOutput, GrantDeleteOutput, GrantListOutput, GrantRowOutput,
};
use hosted_client::hosted_runtime::{
    auth::resolve_server,
    hosted::{HostedAuthMode, HostedClient, HostedSession, canonicalize_spool_path},
};
use wire::ProtocolError;

use super::{
    advice::RecoveryAdvice,
    next_action::{NextActionValidationContext, write_full_command_json},
};
use crate::{
    cli::{
        Cli, CliContext, GrantCommands, GrantCreateArgs, GrantDeleteArgs, GrantListArgs,
        should_output_json, style,
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
        args.role.as_resource_role_name(),
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
        .map(|client| {
            client.with_human_signature_callback(
                hosted_client::client::headless_human_signature_callback(),
            )
        })
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
    let row = grant_row(&created, spool)?;
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

async fn list_grant_rows(client: &mut HostedClient, spool: &str) -> Result<Vec<GrantRowOutput>> {
    // Same bare `spool/<handle>/<name>` CreateGrant/DeleteGrant send.
    // weft exact-matches that visible path; a `repo:` prefix never hits.
    let grants = client
        .list_grants(Some(spool))
        .await
        .map_err(|err| map_grant_error(spool, &err))?;
    grants.iter().map(|grant| grant_row(grant, spool)).collect()
}

async fn list_connected(
    cli: &Cli,
    client: &mut HostedClient,
    server: &str,
    spool: &str,
) -> Result<()> {
    let rows = list_grant_rows(client, spool).await?;
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
            "heddle grant create --spool {spool} --principal <handle> --role writer"
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

fn grant_row(grant: &GrantRecord, spool: &str) -> Result<GrantRowOutput> {
    let reference = grant
        .r#ref
        .as_ref()
        .ok_or_else(|| anyhow!("grant has no stable record ID"))?;
    let role = match ResourceRole::try_from(grant.role) {
        Ok(ResourceRole::Reader) => "reader",
        Ok(ResourceRole::Writer) => "writer",
        Ok(ResourceRole::Administrator) => "administrator",
        _ => return Err(anyhow!("grant has an unknown resource role")),
    };
    let principal = match grant.principal.as_ref() {
        Some(summary) if summary.id == grant.principal_id && !summary.handle.is_empty() => {
            summary.handle.clone()
        }
        _ => grant.principal_id.clone(),
    };
    Ok(GrantRowOutput {
        id: reference.id.clone(),
        principal,
        role: role.into(),
        spool: spool.into(),
    })
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
                return Err(anyhow!(RecoveryAdvice::grant_spool_required()));
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
    let advice = if is_human_verification_required(&lower) {
        RecoveryAdvice::grant_needs_human(spool)
    } else if matches!(
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

fn is_human_verification_required(lowered_message: &str) -> bool {
    lowered_message.contains("user verification required")
        || lowered_message.contains("human verification")
        || lowered_message.contains("webauthn")
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, sync::MutexGuard};

    use api::heddle::api::v2alpha1::{GrantRecord, ResourceRole};
    use clap::Parser;
    use wire::ProtocolError;

    use super::{
        create_connected, delete_connected, grant_row, is_human_verification_required,
        list_grant_rows, map_grant_error, resolve_grant_spool,
    };
    use crate::cli::Cli;

    struct IsolatedHeddleHome {
        _guard: MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        previous_home: Option<OsString>,
    }

    impl IsolatedHeddleHome {
        fn new() -> Self {
            let guard = ::config::credentials::lock_test_env();
            let temp = tempfile::TempDir::new().expect("temporary Heddle home");
            let previous_home = std::env::var_os("HEDDLE_HOME");
            unsafe { std::env::set_var("HEDDLE_HOME", temp.path()) };
            Self {
                _guard: guard,
                _temp: temp,
                previous_home,
            }
        }
    }

    impl Drop for IsolatedHeddleHome {
        fn drop(&mut self) {
            unsafe {
                match &self.previous_home {
                    Some(value) => std::env::set_var("HEDDLE_HOME", value),
                    None => std::env::remove_var("HEDDLE_HOME"),
                }
            }
        }
    }

    #[tokio::test]
    async fn create_then_list_returns_the_grant_row() {
        let _home = IsolatedHeddleHome::new();
        let (mut client, server) =
            hosted_client::hosted_runtime::hosted::test_server::start().await;
        let spool = "spool/willow-ibis-8e7264/notes";
        let cli = Cli::parse_from([
            "heddle",
            "--quiet",
            "grant",
            "create",
            "--spool",
            spool,
            "--principal",
            "alice",
            "--role",
            "writer",
        ]);

        create_connected(&cli, &mut client, "test.invalid", spool, "alice", "writer")
            .await
            .expect("create grant on the production path");

        let rows = list_grant_rows(&mut client, spool)
            .await
            .expect("list grants on the production path");
        assert_eq!(
            rows.len(),
            1,
            "owner list must return the created grant: {rows:?}"
        );
        assert!(uuid::Uuid::parse_str(&rows[0].id).is_ok());
        assert_eq!(rows[0].principal, "alice");
        assert_eq!(rows[0].role, "writer");
        assert_eq!(rows[0].spool, spool);

        delete_connected(&cli, &mut client, "test.invalid", spool, &rows[0].id)
            .await
            .expect("revoke by stable grant ID");
        assert!(
            list_grant_rows(&mut client, spool)
                .await
                .expect("post-revoke list")
                .is_empty()
        );

        client.close().await;
        server.await.unwrap();
    }

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
    fn hosted_url_without_spool_path_is_typed_usage() {
        let err = resolve_grant_spool("https://api.preview.heddle.sh/", None)
            .expect_err("URL without a spool path");
        let advice = err
            .downcast_ref::<crate::cli::commands::RecoveryAdvice>()
            .expect("advice");
        assert_eq!(advice.kind, "grant_spool_required");
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
    fn grant_row_uses_stable_record_id_for_deletion() {
        let row = grant_row(
            &GrantRecord {
                r#ref: Some(api::heddle::api::v2alpha1::RecordRef {
                    spool: Some(api::heddle::api::v2alpha1::SpoolRef {
                        id: uuid::Uuid::from_bytes([2; 16]).to_string(),
                    }),
                    id: uuid::Uuid::from_bytes([3; 16]).to_string(),
                }),
                principal_id: uuid::Uuid::from_bytes([4; 16]).to_string(),
                role: ResourceRole::Writer as i32,
                ..Default::default()
            },
            "spool/me/notes",
        )
        .expect("native grant row");
        assert_eq!(row.id, uuid::Uuid::from_bytes([3; 16]).to_string());
        assert_eq!(row.principal, uuid::Uuid::from_bytes([4; 16]).to_string());
        assert_eq!(row.role, "writer");
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
    fn human_verification_is_not_swallowed_as_a_generic_failure() {
        assert!(is_human_verification_required(
            "user verification required for /heddle.api.v1alpha1.RegistryService/CreateGrant"
        ));
        let err = ProtocolError::AuthorizationFailed(
            "user verification required for CreateGrant: use a client with a WebAuthn authenticator"
                .into(),
        );
        let mapped = map_grant_error("spool/alice/notes", &err);
        let advice = mapped
            .downcast_ref::<crate::cli::commands::RecoveryAdvice>()
            .expect("advice");
        assert_eq!(advice.kind, "grant_needs_human");
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
