// SPDX-License-Identifier: Apache-2.0
//! `heddle prove …` — git-native identity proofs (handle wave F1b).
//!
//! The client half of weft's F1a proof engine. `heddle prove <host> <repo>`
//! mints a challenge via `RequestProofChallenge` and prints the exact marker
//! line + well-known path to publish; publishing (commit + push to your own
//! repo) is *your* action — the CLI never pushes for you. `heddle prove submit
//! <challenge_id>` calls `SubmitProof` and reports the resulting status;
//! `heddle prove list` calls `ListProofs` and renders the caller's proofs.
//!
//! Each subcommand resolves the hosted server from a named remote (mirroring
//! `heddle spool` / `heddle thread approve`) and renders the raw proto reply as
//! text. These are text-output management verbs; they do not advertise a JSON
//! schema.

#![cfg(feature = "client")]

use anyhow::{Result, anyhow};
use grpc::heddle::v1::ProofStatus;
use repo::Repository;

use super::RecoveryAdvice;
use crate::{
    cli::{Cli, ProveArgs, ProveCommands, ProveListArgs, ProveSubmitArgs},
    client::{HostedAuthMode, HostedGrpcClient},
    config::UserConfig,
    remote::{RemoteTarget, resolve_remote_with_key},
};

/// Dispatch `heddle prove [<host> <repo>|submit|list]`.
pub async fn cmd_prove(cli: &Cli, args: ProveArgs) -> Result<()> {
    match args.command.clone() {
        Some(ProveCommands::Submit(sub)) => cmd_prove_submit(cli, sub).await,
        Some(ProveCommands::List(list)) => cmd_prove_list(cli, list).await,
        None => cmd_prove_start(cli, args).await,
    }
}

/// Resolve the named remote to a hosted client. Proofs are a hosted-server
/// concept, so a local remote is a hard error (mirrors spool / thread
/// approvals).
async fn open_hosted_client(repo: &Repository, remote_name: &str) -> Result<HostedGrpcClient> {
    let (target, server_key) = resolve_remote_with_key(repo, Some(remote_name))?;
    let addr = match target {
        RemoteTarget::Network { addr, .. } => addr,
        RemoteTarget::Local(_) => {
            return Err(anyhow!(RecoveryAdvice::safety_refusal(
                "hosted_remote_required",
                format!(
                    "prove operations require a hosted remote; remote '{remote_name}' is local"
                ),
                "Configure a hosted remote or retry against one that resolves to a network target.",
                format!(
                    "remote '{remote_name}' is local, but identity proofs live on the hosted server"
                ),
                "running locally would imply a proof the hosted server never recorded",
                "no hosted request was sent and local repository state was left unchanged",
                "heddle remote list",
                vec!["heddle remote list".to_string()],
            )));
        }
    };

    let user_config = UserConfig::load_default()?;
    // RequestProofChallenge / SubmitProof are pop-tier mutations, so use
    // CredentialFallback (resolves the credential store's proof key) rather than
    // a token-only session, and register the terminal human-signature callback
    // for any human-tier escalation.
    let client = HostedGrpcClient::open_session(
        addr,
        &user_config,
        server_key,
        HostedAuthMode::CredentialFallback,
    )
    .await?
    .with_human_signature_callback(crate::client::cli_human_signature_callback());
    Ok(client)
}

/// Validate the start-form positionals: both `host` and `repo` are required
/// when no subcommand is given. Returns the borrowed pair or a clear error
/// (clap can't express "required unless a subcommand is present" against a
/// `#[command(subcommand)]` field, so we enforce it here).
fn require_host_repo<'a>(
    host: Option<&'a str>,
    repo: Option<&'a str>,
) -> Result<(&'a str, &'a str)> {
    match (host, repo) {
        (Some(h), Some(r)) => Ok((h, r)),
        _ => Err(anyhow!(
            "a host and repo are required (e.g. `heddle prove github.com owner/repo`); \
             for other actions use `heddle prove submit <challenge_id>` or `heddle prove list`"
        )),
    }
}

/// `heddle prove <host> <repo>` — request a challenge and guide publishing.
async fn cmd_prove_start(cli: &Cli, args: ProveArgs) -> Result<()> {
    let (host, repo_arg) = require_host_repo(args.host.as_deref(), args.repo_spec.as_deref())?;

    let repo = cli.open_repo()?;
    let mut client = open_hosted_client(&repo, &args.remote).await?;
    let challenge = client.request_proof_challenge(host, repo_arg).await?;

    // Optional convenience: write the marker file locally so the user can
    // commit + push it. Opt-in and explicit — the CLI never pushes for them.
    if let Some(path) = args.write_file.as_deref() {
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, format!("{}\n", challenge.marker_line))?;
        println!("Wrote marker to {path} (commit + push it yourself).");
        println!();
    }

    println!("Proof challenge started for {repo_arg} on {host}.");
    println!("Challenge id: {}", challenge.challenge_id);
    println!();
    println!("1. Create this file in your repo:");
    println!();
    println!("     {}", challenge.well_known_path);
    println!();
    println!("   with exactly this line:");
    println!();
    println!("     {}", challenge.marker_line);
    println!();
    println!("2. Commit and push it to {repo_arg} (you own the repo — the CLI does not push).");
    println!();
    println!("3. Verify:  heddle prove submit {}", challenge.challenge_id);
    Ok(())
}

/// `heddle prove submit <challenge_id>` — submit for verification.
async fn cmd_prove_submit(cli: &Cli, args: ProveSubmitArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let mut client = open_hosted_client(&repo, &args.remote).await?;
    let response = client.submit_proof(&args.challenge_id).await?;

    let status = ProofStatus::try_from(response.status).unwrap_or(ProofStatus::Unspecified);
    println!("Proof status: {}", status_label(status));
    if !response.detail.is_empty() {
        println!("  {}", response.detail);
    }
    match status {
        ProofStatus::Verified => {
            println!("Your control of the repo is verified.");
        }
        ProofStatus::Pending => {
            println!(
                "The marker was not found yet. Push the file, then retry: heddle prove submit {}",
                args.challenge_id
            );
        }
        ProofStatus::Failed => {
            println!(
                "Verification failed. Check the marker line + path, then retry: heddle prove submit {}",
                args.challenge_id
            );
        }
        ProofStatus::Unspecified => {}
    }
    Ok(())
}

/// `heddle prove list` — list the caller's proofs.
async fn cmd_prove_list(cli: &Cli, args: ProveListArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let mut client = open_hosted_client(&repo, &args.remote).await?;
    let proofs = client.list_proofs().await?;

    if proofs.is_empty() {
        println!("You have no identity proofs.");
        return Ok(());
    }

    println!("{} proof(s):", proofs.len());
    println!(
        "  {host:<20} {repo:<30} {status:<10} verified",
        host = "HOST",
        repo = "REPO",
        status = "STATUS",
    );
    for proof in &proofs {
        let status = ProofStatus::try_from(proof.status).unwrap_or(ProofStatus::Unspecified);
        println!(
            "  {host:<20} {repo:<30} {status:<10} {when}",
            host = proof.host,
            repo = proof.repo,
            status = status_label(status),
            when = ts_label(&proof.verified_at),
        );
    }
    Ok(())
}

fn status_label(status: ProofStatus) -> &'static str {
    match status {
        ProofStatus::Verified => "verified",
        ProofStatus::Pending => "pending",
        ProofStatus::Failed => "failed",
        ProofStatus::Unspecified => "unspecified",
    }
}

fn ts_label(ts: &Option<prost_types::Timestamp>) -> String {
    let secs = ts.as_ref().map(|t| t.seconds.max(0) as u64).unwrap_or(0);
    if secs == 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| secs.to_string())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Commands};

    fn parse_prove(extra: &[&str]) -> Result<ProveArgs, clap::Error> {
        let mut argv: Vec<&str> = vec!["heddle", "prove"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv)?;
        match cli.command {
            Commands::Prove(args) => Ok(args),
            _ => panic!("expected Commands::Prove"),
        }
    }

    #[test]
    fn start_parses_host_and_repo_positionals() {
        let args = parse_prove(&["github.com", "owner/repo"]).expect("host + repo should parse");
        assert!(args.command.is_none());
        assert_eq!(args.host.as_deref(), Some("github.com"));
        assert_eq!(args.repo_spec.as_deref(), Some("owner/repo"));
        assert_eq!(args.remote, "origin");
        assert!(args.write_file.is_none());
    }

    #[test]
    fn start_accepts_write_file_and_remote() {
        let args = parse_prove(&[
            "github.com",
            "owner/repo",
            "--write-file",
            ".well-known/heddle",
            "--remote",
            "upstream",
        ])
        .expect("flags should parse");
        assert_eq!(args.write_file.as_deref(), Some(".well-known/heddle"));
        assert_eq!(args.remote, "upstream");
    }

    #[test]
    fn start_requires_both_positionals() {
        // Bare `heddle prove` and a host-only invocation parse (clap can't gate
        // positionals on a subcommand field), but the runtime start-form guard
        // rejects a missing host or repo with a clear error.
        let bare = parse_prove(&[]).expect("bare prove parses");
        assert!(require_host_repo(bare.host.as_deref(), bare.repo_spec.as_deref()).is_err());

        let host_only = parse_prove(&["github.com"]).expect("host-only parses");
        assert!(
            require_host_repo(host_only.host.as_deref(), host_only.repo_spec.as_deref()).is_err()
        );

        let full = parse_prove(&["github.com", "owner/repo"]).expect("full form parses");
        let (h, r) = require_host_repo(full.host.as_deref(), full.repo_spec.as_deref())
            .expect("full form passes the guard");
        assert_eq!((h, r), ("github.com", "owner/repo"));
    }

    #[test]
    fn submit_parses_challenge_id() {
        let args = parse_prove(&["submit", "chal-123"]).expect("submit should parse");
        match args.command {
            Some(ProveCommands::Submit(sub)) => {
                assert_eq!(sub.challenge_id, "chal-123");
                assert_eq!(sub.remote, "origin");
            }
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn submit_requires_challenge_id() {
        parse_prove(&["submit"]).expect_err("submit without id should fail");
    }

    #[test]
    fn list_parses_with_default_remote() {
        let args = parse_prove(&["list"]).expect("list should parse");
        match args.command {
            Some(ProveCommands::List(list)) => assert_eq!(list.remote, "origin"),
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn status_label_covers_every_variant() {
        assert_eq!(status_label(ProofStatus::Verified), "verified");
        assert_eq!(status_label(ProofStatus::Pending), "pending");
        assert_eq!(status_label(ProofStatus::Failed), "failed");
        assert_eq!(status_label(ProofStatus::Unspecified), "unspecified");
    }
}
