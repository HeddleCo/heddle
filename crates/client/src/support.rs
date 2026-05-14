//! `heddle support` handler — customer-issued temporary admin grants
//! for Heddle staff. Talks to `HostedUserService::{Grant,List,Revoke}
//! SupportAccess` over the configured remote.


use anyhow::{Context, Result, anyhow};
use grpc::heddle::v1::{
    HostedRole, SupportAccessGrant as ProtoSupportAccessGrant,
    grant_target_ref::Target as GrantTargetKind,
};
use repo::Repository;
use serde::Serialize;

use crate::support_args::{
    SupportCommands, SupportGrantArgs, SupportListArgs, SupportRevokeArgs,
};
use crate::grpc_hosted::HostedGrpcClient;
use cli_shared::{
    UserConfig,
    remote::{RemoteTarget, resolve_remote_with_key},
};
use weft_client_shim::CliContext;

#[derive(Serialize)]
struct SupportAccessOutput {
    id: String,
    operator_email: String,
    namespace_path: String,
    repo_path: String,
    role: String,
    granted_by: String,
    granted_at: u64,
    expires_at: u64,
    revoked_at: u64,
    revoked_by: String,
    reason: String,
}

impl From<ProtoSupportAccessGrant> for SupportAccessOutput {
    fn from(g: ProtoSupportAccessGrant) -> Self {
        let (namespace_path, repo_path) = match g.target.and_then(|t| t.target) {
            Some(GrantTargetKind::NamespacePath(p)) => (p, String::new()),
            Some(GrantTargetKind::RepoPath(p)) => (String::new(), p),
            None => (String::new(), String::new()),
        };
        let role = match HostedRole::try_from(g.role).unwrap_or(HostedRole::Unspecified) {
            HostedRole::Reader => "reader",
            HostedRole::Developer => "developer",
            HostedRole::Maintainer => "maintainer",
            HostedRole::Admin => "admin",
            HostedRole::Owner => "owner",
            HostedRole::Unspecified => "",
        }
        .to_string();
        Self {
            id: g.id,
            operator_email: g.operator_email,
            namespace_path,
            repo_path,
            role,
            granted_by: g.granted_by,
            granted_at: g
                .granted_at
                .as_ref()
                .map(|t| t.seconds.max(0) as u64)
                .unwrap_or(0),
            expires_at: g
                .expires_at
                .as_ref()
                .map(|t| t.seconds.max(0) as u64)
                .unwrap_or(0),
            revoked_at: g
                .revoked_at
                .as_ref()
                .map(|t| t.seconds.max(0) as u64)
                .unwrap_or(0),
            revoked_by: g.revoked_by,
            reason: g.reason,
        }
    }
}

pub async fn run(ctx: &dyn CliContext, command: SupportCommands) -> Result<()> {
    match command {
        SupportCommands::Grant(args) => run_grant(ctx, args).await,
        SupportCommands::List(args) => run_list(ctx, args).await,
        SupportCommands::Revoke(args) => run_revoke(ctx, args).await,
    }
}

/// Resolve the working repo path from `CliContext`. `--repo` override
/// wins; falling back to the cwd is fallible (the cwd can be deleted or
/// permission-denied) so we propagate the error rather than panicking.
fn resolve_repo_path(ctx: &dyn CliContext) -> Result<std::path::PathBuf> {
    match ctx.repo_path() {
        Some(p) => Ok(p.to_path_buf()),
        None => std::env::current_dir().map_err(anyhow::Error::from),
    }
}

async fn open_client(repo: &Repository, remote: &str) -> Result<HostedGrpcClient> {
    let (target, server_key) =
        resolve_remote_with_key(repo, Some(remote)).map_err(anyhow::Error::msg)?;
    let addr = match target {
        RemoteTarget::Network { addr, .. } => addr,
        RemoteTarget::Local(_) => {
            return Err(anyhow!(
                "support access is a hosted-server feature; remote '{remote}' is local"
            ));
        }
    };
    let user_config = UserConfig::load_default().unwrap_or_default();
    let token = user_config.remote_token();
    let mut config = user_config.heddle_client_config(token);
    if let Some(key) = server_key {
        config = config.with_server_key(key);
    }
    let mut client = HostedGrpcClient::connect(addr, &config).await?;
    client.auto_rotate_if_needed().await;
    Ok(client)
}

/// Parse a TTL string like `"24h"`, `"30m"`, `"4d"`, or a bare number
/// of seconds. Accepts a single suffix token: `s`, `m`, `h`, `d`.
/// Anything else fails with a clear error.
fn parse_ttl(raw: &str) -> Result<u32> {
    let raw = raw.trim();
    let (num_part, multiplier_secs) = match raw.chars().last() {
        Some(c) if c.is_ascii_digit() => (raw, 1u64),
        Some('s') => (&raw[..raw.len() - 1], 1u64),
        Some('m') => (&raw[..raw.len() - 1], 60u64),
        Some('h') => (&raw[..raw.len() - 1], 3600u64),
        Some('d') => (&raw[..raw.len() - 1], 86400u64),
        _ => {
            return Err(anyhow!(
                "invalid --ttl '{raw}': use 30s/15m/2h/3d or seconds"
            ));
        }
    };
    let n: u64 = num_part
        .parse()
        .with_context(|| format!("invalid --ttl '{raw}'"))?;
    let total = n
        .checked_mul(multiplier_secs)
        .ok_or_else(|| anyhow!("--ttl too large"))?;
    if total == 0 {
        return Err(anyhow!("--ttl must be > 0"));
    }
    let secs: u32 = total.try_into().map_err(|_| anyhow!("--ttl too large"))?;
    Ok(secs)
}

async fn run_grant(ctx: &dyn CliContext, args: SupportGrantArgs) -> Result<()> {
    if args.namespace.is_none() && args.repo.is_none() {
        return Err(anyhow!("one of --namespace or --repo is required"));
    }
    let repo = Repository::open(resolve_repo_path(ctx)?)?;
    let mut client = open_client(&repo, &args.remote).await?;
    let ttl_secs = parse_ttl(&args.ttl)?;
    let op_id = ctx.operation_id_wire();
    let grant = client
        .grant_support_access(
            &args.operator_email,
            args.namespace.as_deref(),
            args.repo.as_deref(),
            ttl_secs,
            &args.reason,
            op_id,
        )
        .await?;
    let out: SupportAccessOutput = grant.into();
    if ctx.should_output_json(Some(repo.config())) {
        println!("{}", serde_json::to_string(&out)?);
    } else {
        let target = if !out.namespace_path.is_empty() {
            format!("namespace {}", out.namespace_path)
        } else {
            format!("repo {}", out.repo_path)
        };
        let expires = chrono::DateTime::from_timestamp(out.expires_at as i64, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| out.expires_at.to_string());
        println!(
            "Granted support access to {} on {} (expires {expires}).",
            out.operator_email, target
        );
        println!("  id:     {}", out.id);
        println!("  reason: {}", out.reason);
    }
    Ok(())
}

async fn run_list(ctx: &dyn CliContext, args: SupportListArgs) -> Result<()> {
    if args.namespace.is_none() && args.repo.is_none() {
        return Err(anyhow!("one of --namespace or --repo is required"));
    }
    let repo = Repository::open(resolve_repo_path(ctx)?)?;
    let mut client = open_client(&repo, &args.remote).await?;
    let grants = client
        .list_support_access_grants(
            args.namespace.as_deref(),
            args.repo.as_deref(),
            args.include_inactive,
        )
        .await?;
    let entries: Vec<SupportAccessOutput> = grants.into_iter().map(Into::into).collect();
    if ctx.should_output_json(Some(repo.config())) {
        println!("{}", serde_json::to_string(&entries)?);
    } else if entries.is_empty() {
        println!("No support-access grants on the requested resource.");
    } else {
        println!("{} grant(s):", entries.len());
        for g in entries {
            let target = if !g.namespace_path.is_empty() {
                g.namespace_path.as_str()
            } else {
                g.repo_path.as_str()
            };
            let status = if g.revoked_at > 0 {
                let revoked_by = if g.revoked_by.is_empty() {
                    "?".to_string()
                } else {
                    g.revoked_by
                };
                format!("revoked by {revoked_by}")
            } else {
                let expires = chrono::DateTime::from_timestamp(g.expires_at as i64, 0)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(|| g.expires_at.to_string());
                format!("active (expires {expires})")
            };
            println!(
                "  {id}  {email}  {target}  [{status}]  reason={reason}",
                id = g.id,
                email = g.operator_email,
                reason = g.reason,
            );
        }
    }
    Ok(())
}

async fn run_revoke(ctx: &dyn CliContext, args: SupportRevokeArgs) -> Result<()> {
    let repo = Repository::open(resolve_repo_path(ctx)?)?;
    let mut client = open_client(&repo, &args.remote).await?;
    let op_id = ctx.operation_id_wire();
    client.revoke_support_access(&args.id, op_id).await?;
    if ctx.should_output_json(Some(repo.config())) {
        println!("{{\"revoked\":\"{}\"}}", args.id);
    } else {
        println!("Revoked support-access grant {}.", args.id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_ttl;

    #[test]
    fn parse_ttl_round_trips_canonical_suffixes() {
        assert_eq!(parse_ttl("30s").unwrap(), 30);
        assert_eq!(parse_ttl("15m").unwrap(), 15 * 60);
        assert_eq!(parse_ttl("2h").unwrap(), 2 * 3600);
        assert_eq!(parse_ttl("3d").unwrap(), 3 * 86400);
        assert_eq!(parse_ttl("90").unwrap(), 90);
    }

    #[test]
    fn parse_ttl_rejects_bad_input() {
        assert!(parse_ttl("0").is_err());
        assert!(parse_ttl("1y").is_err());
        assert!(parse_ttl("h").is_err());
        assert!(parse_ttl("").is_err());
    }
}
