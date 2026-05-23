// SPDX-License-Identifier: Apache-2.0
//! Client-supplied operation-id resolution.
//!
//! The CLI accepts `--op-id <UUID>` (or `HEDDLE_OPERATION_ID`) on every
//! state-changing verb. When set, the verb passes it through to the
//! gRPC layer; the dedup store returns the original outcome on replay.
//! When unset, the call executes without dedup.
//!
//! Verbs that have been routed through `with_idempotency` in the
//! `grpc_local_impl` services already honour the field. Verbs that
//! still bypass the gRPC layer (most existing core verbs) ignore it
//! today; wiring lands incrementally as those verbs migrate.

use std::{io::Write, path::PathBuf, process::Command, str::FromStr};

use anyhow::{Context, Result, anyhow};
use objects::object::OperationId;
use repo::{
    Repository,
    operation_dedup::{DedupOutcome, OperationDedupStore, hash_request_body},
};
use serde::{Deserialize, Serialize};

use crate::cli::{cli_args::Cli, commands::RecoveryAdvice};

const LOCAL_OP_ID_CHILD_ENV: &str = "HEDDLE_LOCAL_OP_ID_CHILD";

/// Canonical helper used by every state-changing dispatch arm in
/// `main.rs`. Validates the `--op-id` format eagerly so a malformed
/// value fails before the verb starts work.
///
/// The `op_id_coverage` build-time test grep-asserts a call to this
/// function in every state-changing arm — keep the name stable.
pub fn resolve_operation_id(cli: &Cli) -> Result<Option<OperationId>> {
    let Some(raw) = cli.op_id.as_deref() else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(
        OperationId::from_str(raw).context("parse --op-id as UUID v4")?,
    ))
}

pub fn supports_local_op_id(command_name: &str) -> bool {
    crate::cli::commands::command_supports_op_id(command_name)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalOpIdResponse {
    status_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

pub fn run_local_idempotency_if_requested(
    cli: &Cli,
    command_name: &str,
    command_supports_op_id: bool,
) -> Result<bool> {
    let Some(op_id) = resolve_operation_id(cli)? else {
        return Ok(false);
    };
    if std::env::var_os(LOCAL_OP_ID_CHILD_ENV).is_some() {
        return Ok(false);
    }
    if !command_supports_op_id {
        return Err(anyhow!(RecoveryAdvice::op_id_unsupported(command_name)));
    }

    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let normalized_args = normalized_argv_for_op_id();
    let request_hash = hash_request_body(normalized_args.join("\0").as_bytes());
    let store = OperationDedupStore::open(repo.heddle_dir()).context("open op-id dedup store")?;

    match store.reserve(op_id, command_name, request_hash)? {
        DedupOutcome::Replay { response } => {
            let replay: LocalOpIdResponse =
                serde_json::from_slice(&response).context("decode cached op-id response")?;
            replay_response(&replay)?;
            if replay.status_code != 0 {
                std::process::exit(replay.status_code);
            }
            Ok(true)
        }
        DedupOutcome::Conflict => Err(anyhow!(RecoveryAdvice::op_id_conflict())),
        DedupOutcome::InFlight => Err(anyhow!(RecoveryAdvice::op_id_in_flight())),
        DedupOutcome::Reserved => {
            let output = Command::new(std::env::current_exe()?)
                .args(std::env::args_os().skip(1))
                .env(LOCAL_OP_ID_CHILD_ENV, "1")
                .output();
            let output = match output {
                Ok(output) => output,
                Err(error) => {
                    let _ = store.cancel(op_id, command_name);
                    return Err(error).context("run op-id child process");
                }
            };
            let response = LocalOpIdResponse {
                status_code: output.status.code().unwrap_or(1),
                stdout: output.stdout,
                stderr: output.stderr,
            };
            let encoded = serde_json::to_vec(&response).context("encode cached op-id response")?;
            store.record(op_id, command_name, request_hash, encoded)?;
            replay_response(&response)?;
            if response.status_code != 0 {
                std::process::exit(response.status_code);
            }
            Ok(true)
        }
    }
}

fn replay_response(response: &LocalOpIdResponse) -> Result<()> {
    std::io::stdout().write_all(&response.stdout)?;
    std::io::stderr().write_all(&response.stderr)?;
    Ok(())
}

fn normalized_argv_for_op_id() -> Vec<String> {
    let mut normalized = Vec::new();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        if arg == "--op-id" {
            let _ = args.next();
            continue;
        }
        if arg.starts_with("--op-id=") {
            continue;
        }
        normalized.push(arg);
    }
    normalized
}

/// Same as [`resolve_operation_id`] but returns the wire-string form
/// expected by gRPC requests. `""` means "no idempotency for this call".
pub fn wire(cli: &Cli) -> String {
    cli.op_id.clone().unwrap_or_default()
}

/// Per-repo session directory under `$HOME/.heddle/session/<repo-id>`.
/// `<repo-id>` is a 16-char SHA-256 of the canonical repo root so two
/// worktrees of the same repo don't collide.
fn session_dir_for(repo: &Repository) -> PathBuf {
    use sha2::{Digest, Sha256};
    let canonical =
        std::fs::canonicalize(repo.root()).unwrap_or_else(|_| repo.root().to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hex::encode(hasher.finalize());
    let repo_id = &digest[..16.min(digest.len())];
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(".heddle").join("session").join(repo_id)
}

fn last_op_id_path(repo: &Repository) -> PathBuf {
    session_dir_for(repo).join("last_op_id.toml")
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct LastOpIdFile {
    /// Per-verb most-recent op-id. Verbs whose command contract does
    /// not opt into op-id persistence are never read or written here.
    #[serde(default)]
    by_verb: std::collections::BTreeMap<String, String>,
}

/// Resolve the op-id for a verb that opts into `^C → re-run`
/// persistence. Order:
///   1. Caller passed `--op-id` / `HEDDLE_OPERATION_ID` → use it.
///   2. The command contract opts into op-id persistence AND a recent
///      saved id exists for that verb → use it (don't persist; we're
///      reusing).
///   3. Otherwise generate a fresh id, persist it for the verb, return.
///
/// Call [`clear_persisted_op_id`] after the verb completes
/// successfully so the next run gets a fresh id.
pub fn resolve_or_persist_for_verb(
    cli: &Cli,
    repo: &Repository,
    verb: &str,
) -> Result<OperationId> {
    if let Some(explicit) = resolve_operation_id(cli)? {
        return Ok(explicit);
    }
    if !crate::cli::commands::command_persists_op_id(verb) {
        return Ok(OperationId::new());
    }
    let path = last_op_id_path(repo);
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(decoded) = toml::from_str::<LastOpIdFile>(&String::from_utf8_lossy(&bytes))
        && let Some(saved) = decoded.by_verb.get(verb)
        && let Ok(parsed) = OperationId::from_str(saved)
    {
        return Ok(parsed);
    }
    let fresh = OperationId::new();
    persist_op_id(&path, verb, &fresh).context("persist last op id")?;
    Ok(fresh)
}

/// Drop the persisted op-id for `verb`. Called after a successful
/// response — releases the slot so the next run gets a fresh id
/// rather than replaying.
pub fn clear_persisted_op_id(repo: &Repository, verb: &str) -> Result<()> {
    let path = last_op_id_path(repo);
    let mut file: LastOpIdFile = match std::fs::read(&path) {
        Ok(bytes) => toml::from_str(&String::from_utf8_lossy(&bytes)).unwrap_or_default(),
        Err(_) => return Ok(()),
    };
    if file.by_verb.remove(verb).is_none() {
        return Ok(());
    }
    if file.by_verb.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let serialized = toml::to_string(&file).context("serialize last_op_id.toml")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serialized)?;
    Ok(())
}

fn persist_op_id(path: &std::path::Path, verb: &str, op_id: &OperationId) -> Result<()> {
    let mut file: LastOpIdFile = match std::fs::read(path) {
        Ok(bytes) => toml::from_str(&String::from_utf8_lossy(&bytes)).unwrap_or_default(),
        Err(_) => LastOpIdFile::default(),
    };
    file.by_verb.insert(verb.to_string(), op_id.to_string());
    let serialized = toml::to_string(&file).context("serialize last_op_id.toml")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serialized)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_with(op_id: Option<&str>) -> Cli {
        let mut cli: Cli = clap::Parser::parse_from(["heddle", "status"]);
        cli.op_id = op_id.map(|s| s.to_string());
        cli
    }

    #[test]
    fn resolve_none_when_unset() {
        let cli = cli_with(None);
        assert!(resolve_operation_id(&cli).unwrap().is_none());
    }

    #[test]
    fn resolve_parses_uuid() {
        let id = OperationId::new();
        let cli = cli_with(Some(&id.to_string()));
        assert_eq!(resolve_operation_id(&cli).unwrap(), Some(id));
    }

    #[test]
    fn resolve_rejects_garbage() {
        let cli = cli_with(Some("not-a-uuid"));
        assert!(resolve_operation_id(&cli).is_err());
    }

    #[test]
    fn wire_is_empty_when_unset() {
        let cli = cli_with(None);
        assert_eq!(wire(&cli), "");
    }

    #[test]
    fn wire_returns_string_when_set() {
        let id = OperationId::new();
        let cli = cli_with(Some(&id.to_string()));
        assert_eq!(wire(&cli), id.to_string());
    }
}
