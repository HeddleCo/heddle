// SPDX-License-Identifier: Apache-2.0
//! Explicit existing-credential exchange. No enrollment, discovery or persistence.
use anyhow::{Result, bail, ensure};
use biscuit_verifier::git_transport::{GitAction, GitScope};
use crypto::Ed25519Signer;
use hosted_client::git_transport::{GitTransportClient, GitTransportGrant};
use serde::Deserialize;
use std::{
    ffi::OsString,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

const HELP: &str = "Usage: git_transport_session --authority HTTPS_ORIGIN --scope-file FILE --biscuit-file FILE --proof-key-file FILE\n\nExchange an explicitly supplied existing Ed25519 account/agent credential for a\nshort-lived Git session. No enrollment, saved identity lookup, or renewal.\nStdout is ONLY the secret session token plus newline. Capture it privately; do\nnot log it, put it in a URL, or save it in Git configuration. Use HTTPS Git Basic\nwith username git and this token as its transient password. Scope JSON fields:\nservice_audience, tenant_spool_id, spool_id, repository_path, thread_id (64 lower\nhex characters), action (read or write), disclosure_audience. Secret input files\nmust be regular files with owner-only permissions on Unix; symlinks are refused.\n";

// No Debug: paths identify explicitly supplied credential material.
struct Args {
    authority: String,
    scope: PathBuf,
    biscuit: PathBuf,
    proof_key: PathBuf,
}
enum Command {
    Help,
    Exchange(Args),
}
fn arguments(values: Vec<OsString>) -> Result<Command> {
    if values.len() == 1 && values[0] == "--help" {
        return Ok(Command::Help);
    }
    ensure!(
        values.len() == 8,
        "exactly four explicit options are required; use --help"
    );
    let (mut authority, mut scope, mut biscuit, mut proof_key) = (None, None, None, None);
    for pair in values.chunks_exact(2) {
        ensure!(!pair[1].is_empty(), "option value is missing");
        match pair[0].to_str() {
            Some("--authority") if authority.is_none() => {
                authority = Some(
                    pair[1]
                        .clone()
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("authority must be UTF-8"))?,
                )
            }
            Some("--scope-file") if scope.is_none() => scope = Some(PathBuf::from(&pair[1])),
            Some("--biscuit-file") if biscuit.is_none() => biscuit = Some(PathBuf::from(&pair[1])),
            Some("--proof-key-file") if proof_key.is_none() => {
                proof_key = Some(PathBuf::from(&pair[1]))
            }
            _ => bail!("unknown or duplicate option; use --help"),
        }
    }
    Ok(Command::Exchange(Args {
        authority: authority.ok_or_else(|| anyhow::anyhow!("authority required"))?,
        scope: scope.ok_or_else(|| anyhow::anyhow!("scope file required"))?,
        biscuit: biscuit.ok_or_else(|| anyhow::anyhow!("Biscuit file required"))?,
        proof_key: proof_key.ok_or_else(|| anyhow::anyhow!("proof key file required"))?,
    }))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeFile {
    service_audience: String,
    tenant_spool_id: String,
    spool_id: String,
    repository_path: String,
    thread_id: String,
    action: String,
    disclosure_audience: String,
}
fn scope(bytes: &[u8]) -> Result<GitScope> {
    ensure!(bytes.len() <= 8192, "scope file exceeds limit");
    let value: ScopeFile =
        serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("invalid scope JSON"))?;
    let tenant = uuid::Uuid::parse_str(&value.tenant_spool_id)
        .map_err(|_| anyhow::anyhow!("invalid tenant UUID"))?;
    let spool = uuid::Uuid::parse_str(&value.spool_id)
        .map_err(|_| anyhow::anyhow!("invalid Spool UUID"))?;
    ensure!(
        tenant.to_string() == value.tenant_spool_id && spool.to_string() == value.spool_id,
        "canonical UUIDs required"
    );
    ensure!(
        value.thread_id.len() == 64
            && value
                .thread_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "canonical Thread hex required"
    );
    let thread = hex::decode(&value.thread_id).map_err(|_| anyhow::anyhow!("invalid Thread"))?;
    let scope = GitScope {
        service_audience: value.service_audience,
        tenant_spool_id: tenant,
        spool_id: spool,
        repository_path: value.repository_path,
        thread_id: thread
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid Thread width"))?,
        action: match value.action.as_str() {
            "read" => GitAction::Read,
            "write" => GitAction::Write,
            _ => bail!("scope action must be read or write"),
        },
        disclosure_audience: value.disclosure_audience,
    };
    scope
        .validate()
        .map_err(|_| anyhow::anyhow!("invalid canonical Git scope"))?;
    Ok(scope)
}
fn read(path: &Path, maximum: usize, private: bool) -> Result<Vec<u8>> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| anyhow::anyhow!("input file unavailable"))?;
    ensure!(
        metadata.file_type().is_file() && metadata.len() <= maximum as u64,
        "regular bounded input file required"
    );
    if private {
        crypto::reject_group_or_world_readable_key(path)
            .map_err(|_| anyhow::anyhow!("secret input requires owner-only file permissions"))?;
    }
    let file = File::open(path).map_err(|_| anyhow::anyhow!("input file unavailable"))?;
    let mut bytes = Vec::new();
    file.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("input file unreadable"))?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= maximum,
        "empty or oversized input file"
    );
    Ok(bytes)
}
async fn exchange(args: Args) -> Result<GitTransportGrant> {
    // Validate the destination before opening any credential file.
    let client = GitTransportClient::new(&args.authority)?;
    let scope = scope(&read(&args.scope, 8192, false)?)?;
    let biscuit = String::from_utf8(read(&args.biscuit, 64 * 1024, true)?)
        .map_err(|_| anyhow::anyhow!("invalid credential encoding"))?;
    let pem = String::from_utf8(read(&args.proof_key, 16 * 1024, true)?)
        .map_err(|_| anyhow::anyhow!("invalid proof key encoding"))?;
    let signer = Ed25519Signer::from_pem(&pem)
        .map_err(|_| anyhow::anyhow!("existing Ed25519 PKCS#8 proof key required"))?;
    client.exchange(&scope, biscuit.trim(), &signer).await
}
fn run() -> Result<()> {
    match arguments(std::env::args_os().skip(1).collect())? {
        Command::Help => {
            std::io::stdout().lock().write_all(HELP.as_bytes())?;
        }
        Command::Exchange(args) => {
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let grant = executor.block_on(exchange(args))?;
            // The sole intentional secret output. No diagnostics or persistence.
            let mut output = std::io::stdout().lock();
            output.write_all(grant.token().as_bytes())?;
            output.write_all(b"\n")?;
        }
    }
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => {
            // Do not format nested parser/network errors or caller inputs.
            eprintln!(
                "Git session exchange failed. Check explicit options (--help), protected input files, exact scope, proof key and current authority."
            );
            ExitCode::FAILURE
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn valid_authority_constructor_in_fresh_process() {
        const CHILD: &str = "HEDDLE_GIT_SESSION_CONSTRUCTOR_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert!(rustls::crypto::CryptoProvider::get_default().is_none());
            assert!(GitTransportClient::new("https://authority.example/").is_ok());
            assert!(rustls::crypto::CryptoProvider::get_default().is_some());
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "tests::valid_authority_constructor_in_fresh_process",
            ])
            .env(CHILD, "1")
            .output()
            .expect("fresh test process");
        assert!(
            output.status.success(),
            "fresh constructor process failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn argv(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }
    fn input() -> serde_json::Value {
        serde_json::json!({"service_audience":"git-gateway","tenant_spool_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","spool_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","repository_path":"example/repository","thread_id":"11".repeat(32),"action":"write","disclosure_audience":"public"})
    }
    #[test]
    fn only_explicit_unique_file_options_and_help_are_accepted() {
        assert!(matches!(
            arguments(argv(&["--help"])).expect("help"),
            Command::Help
        ));
        assert!(matches!(
            arguments(argv(&[
                "--authority",
                "https://authority.example/",
                "--scope-file",
                "scope.json",
                "--biscuit-file",
                "existing.biscuit",
                "--proof-key-file",
                "existing.pem"
            ]))
            .expect("options"),
            Command::Exchange(_)
        ));
        for values in [
            vec![],
            vec!["--help", "extra"],
            vec!["--authority", "http://example"],
            vec![
                "--authority",
                "https://example",
                "--scope-file",
                "scope",
                "--biscuit-file",
                "credential",
                "--token",
                "SECRET",
            ],
            vec![
                "--authority",
                "https://example",
                "--scope-file",
                "scope",
                "--biscuit-file",
                "credential",
                "--biscuit-file",
                "duplicate",
            ],
        ] {
            let error = arguments(argv(&values)).err().expect("refused").to_string();
            assert!(!error.contains("SECRET"));
        }
    }
    #[test]
    fn scope_is_strict_canonical_and_never_infers_missing_dimensions() {
        assert_eq!(
            scope(&serde_json::to_vec(&input()).expect("JSON"))
                .expect("scope")
                .action,
            GitAction::Write
        );
        for (field, value) in [
            ("service_audience", "public"),
            ("tenant_spool_id", "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA"),
            ("spool_id", "00000000-0000-0000-0000-000000000000"),
            ("thread_id", "00"),
            ("action", "admin"),
            ("repository_path", "me/repository"),
            ("disclosure_audience", "unknown"),
        ] {
            let mut value_json = input();
            value_json[field] = value.into();
            assert!(scope(&serde_json::to_vec(&value_json).expect("JSON")).is_err());
        }
        let mut unknown = input();
        unknown["actor"] = "caller-asserted".into();
        assert!(scope(&serde_json::to_vec(&unknown).expect("JSON")).is_err());
        let duplicate = format!(
            "{{\"action\":\"read\",{}",
            &serde_json::to_string(&input()).expect("JSON")[1..]
        );
        assert!(scope(duplicate.as_bytes()).is_err());
        assert!(scope(&vec![b' '; 8193]).is_err());
    }
    #[tokio::test]
    async fn unsafe_authority_is_refused_before_any_credential_file_is_opened() {
        for authority in [
            "http://authority.example/",
            "https://user:SECRET@authority.example/",
            "https://authority.example/elsewhere",
            "https://authority.example/?credential=SECRET",
            "https://authority.example/#fragment",
        ] {
            let error = exchange(Args {
                authority: authority.into(),
                scope: "/absent/scope".into(),
                biscuit: "/absent/credential".into(),
                proof_key: "/absent/key".into(),
            })
            .await
            .err()
            .expect("unsafe destination")
            .to_string();
            assert!(error.contains("HTTPS origin"));
            assert!(!error.contains("SECRET"));
            assert!(!error.contains("input file"));
        }
    }
    #[test]
    fn bounded_regular_inputs_reject_directories_symlinks_and_exposed_secrets() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("synthetic");
        std::fs::write(&path, b"synthetic").expect("write");
        assert_eq!(read(&path, 9, false).expect("bounded"), b"synthetic");
        assert!(read(&path, 8, false).is_err());
        assert!(read(directory.path(), 4096, false).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                .expect("permissions");
            assert!(read(&path, 9, true).is_err());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("permissions");
            assert!(read(&path, 9, true).is_ok());
            let alias = directory.path().join("alias");
            symlink(&path, &alias).expect("symlink");
            assert!(read(&alias, 9, true).is_err());
        }
    }
}
