// SPDX-License-Identifier: Apache-2.0
//! Client-supplied operation-id resolution.
//!
//! Commands that advertise `supports_op_id: true` in the command
//! catalog accept `--op-id <UUID>` or `HEDDLE_OPERATION_ID`. The local
//! CLI reserves the id before dispatch, records stdout/stderr/exit
//! status, and replays that recorded result for the same command body.
//! Reusing the id with different arguments fails with a typed conflict.
//!
//! Commands that advertise `persists_op_id: true` may additionally
//! generate and save an op-id for interrupted retry loops. Current
//! explicit replay support does not imply generated persistence.

use std::{io::Write, path::PathBuf, process::Command, str::FromStr, sync::Arc};

use anyhow::{Context, Result, anyhow};
use objects::object::OperationId;
use repo::{
    Repository,
    operation_dedup::{
        DedupOutcome, OperationDedupStore, hash_request_body, reserve_operation_id_eager,
    },
};
use serde::{Deserialize, Serialize};

use crate::cli::{
    cli_args::{Cli, OutputMode},
    commands::RecoveryAdvice,
};

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
    OperationId::from_str(raw)
        .map(Some)
        .map_err(|err| anyhow!(RecoveryAdvice::op_id_invalid(raw, err)))
}

pub fn supports_local_op_id(command_name: &str) -> bool {
    crate::cli::commands::command_runtime_contract(command_name)
        .map(|contract| contract.supports_op_id)
        .unwrap_or(false)
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

    let bootstrap_store = uses_bootstrap_op_id_store(command_name);
    let normalized_args = normalized_argv_for_op_id();
    let bootstrap_scope = if bootstrap_store {
        Some(bootstrap_op_id_scope(cli)?)
    } else {
        None
    };
    let request_hash = request_hash_for_op_id(
        &normalized_args,
        bootstrap_scope
            .as_ref()
            .map(|scope| scope.hash_material.as_str()),
    )?;
    let repo_for_eager;
    let store = if bootstrap_store {
        let scope = bootstrap_scope
            .as_ref()
            .expect("bootstrap scope should be present for bootstrap store");
        repo_for_eager = None;
        Arc::new(
            OperationDedupStore::open(bootstrap_op_id_store_dir(scope))
                .context("open bootstrap op-id dedup store")?,
        )
    } else {
        let repo = cli.open_repo()?;
        let bootstrap_scope = bootstrap_op_id_scope_for_root(repo.root().to_path_buf())?;
        let bootstrap_store =
            OperationDedupStore::open(bootstrap_op_id_store_dir(&bootstrap_scope))
                .context("open bootstrap op-id dedup store")?;
        if let Some(existing) = bootstrap_store.metadata_for(op_id, command_name) {
            return Err(anyhow!(RecoveryAdvice::op_id_conflict(
                command_name,
                &bootstrap_scope.label,
                &normalized_args,
                request_hash,
                Some(existing),
            )));
        }
        let store = Arc::new(
            OperationDedupStore::open(repo.heddle_dir()).context("open op-id dedup store")?,
        );
        repo_for_eager = Some(repo);
        store
    };
    let json_mode = explicit_json_requested(cli);

    let reserve_outcome = if let Some(repo) = repo_for_eager.as_ref() {
        reserve_operation_id_eager(repo, Arc::clone(&store), op_id, command_name, request_hash)?
    } else {
        store.reserve(op_id, command_name, request_hash)?
    };

    match reserve_outcome {
        DedupOutcome::Replay { response } => {
            let replay: LocalOpIdResponse =
                serde_json::from_slice(&response).context("decode cached op-id response")?;
            replay_response(
                &replay,
                json_mode.then_some(OpIdDisplayContext {
                    op_id: &op_id,
                    command_name,
                    status: "replayed",
                    replayed: true,
                }),
            )?;
            if replay.status_code != 0 {
                std::process::exit(replay.status_code);
            }
            Ok(true)
        }
        DedupOutcome::Conflict => Err(anyhow!(RecoveryAdvice::op_id_conflict(
            command_name,
            bootstrap_scope
                .as_ref()
                .map(|scope| scope.label.as_str())
                .unwrap_or("repository-local .heddle"),
            &normalized_args,
            request_hash,
            store.metadata_for(op_id, command_name),
        ))),
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
            replay_response(
                &response,
                json_mode.then_some(OpIdDisplayContext {
                    op_id: &op_id,
                    command_name,
                    status: "executed",
                    replayed: false,
                }),
            )?;
            if response.status_code != 0 {
                std::process::exit(response.status_code);
            }
            Ok(true)
        }
    }
}

#[derive(Clone, Copy)]
struct OpIdDisplayContext<'a> {
    op_id: &'a OperationId,
    command_name: &'a str,
    status: &'a str,
    replayed: bool,
}

fn replay_response(
    response: &LocalOpIdResponse,
    context: Option<OpIdDisplayContext>,
) -> Result<()> {
    let stdout = context
        .map(|context| decorate_json_stream(&response.stdout, context))
        .transpose()?
        .unwrap_or_else(|| response.stdout.clone());
    let stderr = context
        .map(|context| decorate_json_stream(&response.stderr, context))
        .transpose()?
        .unwrap_or_else(|| response.stderr.clone());
    std::io::stdout().write_all(&stdout)?;
    std::io::stderr().write_all(&stderr)?;
    Ok(())
}

fn explicit_json_requested(cli: &Cli) -> bool {
    matches!(cli.output, Some(OutputMode::Json | OutputMode::JsonCompact))
}

fn decorate_json_stream(bytes: &[u8], context: OpIdDisplayContext) -> Result<Vec<u8>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Ok(bytes.to_vec());
    };
    let Some(object) = value.as_object_mut() else {
        return Ok(bytes.to_vec());
    };
    let op_id = context.op_id.to_string();
    object.insert(
        "op_id".to_string(),
        serde_json::Value::String(op_id.clone()),
    );
    object.insert(
        "idempotency_status".to_string(),
        serde_json::Value::String(context.status.to_string()),
    );
    object.insert(
        "replayed".to_string(),
        serde_json::Value::Bool(context.replayed),
    );
    object.insert(
        "operation_record".to_string(),
        serde_json::json!({
            "op_id": op_id,
            "command": context.command_name,
            "idempotency_status": context.status,
            "replayed": context.replayed,
        }),
    );
    let mut decorated = serde_json::to_vec(&value)?;
    decorated.push(b'\n');
    Ok(decorated)
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

fn request_hash_for_op_id(
    normalized_args: &[String],
    invocation_context: Option<&str>,
) -> Result<[u8; 32]> {
    let mut body = normalized_args.join("\0").into_bytes();
    if let Some(context) = invocation_context {
        body.extend_from_slice(b"\0context\0");
        body.extend_from_slice(context.as_bytes());
    }
    Ok(hash_request_body(&body))
}

fn uses_bootstrap_op_id_store(command_name: &str) -> bool {
    crate::cli::commands::command_uses_bootstrap_op_id_store(command_name)
}

struct BootstrapOpIdScope {
    id: String,
    label: String,
    hash_material: String,
}

fn bootstrap_op_id_scope(cli: &Cli) -> Result<BootstrapOpIdScope> {
    let root = match &cli.command {
        crate::cli::Commands::Init(args) => args.path.clone().or_else(|| cli.repo.clone()),
        crate::cli::Commands::Adopt(args) => args.path.clone().or_else(|| cli.repo.clone()),
        // Clone destinations normally don't exist yet, so feeding the
        // raw string into the hasher (and relying on the canonicalize
        // fallback) lets two different cwds with `./repo` collide in
        // the bootstrap cache. Resolve the destination against the
        // current directory up front so the scope is cwd-specific.
        crate::cli::Commands::Clone(args) => {
            let cwd = std::env::current_dir()
                .context("resolve current directory for clone op-id scope")?;
            Some(absolutize_clone_destination(&args.local, &cwd))
        }
        _ => cli.repo.clone(),
    }
    .unwrap_or(std::env::current_dir().context("resolve current directory for op-id scope")?);
    bootstrap_op_id_scope_for_root(root)
}

/// Anchor a (possibly relative, possibly non-existent) clone destination
/// to an absolute path that's stable across cwds. `canonicalize` only
/// works on paths that exist, so for the typical `heddle clone ./repo`
/// case we walk up to the longest existing prefix, canonicalize it, then
/// re-attach the remainder verbatim.
fn absolutize_clone_destination(dest: &str, cwd: &std::path::Path) -> PathBuf {
    let path = PathBuf::from(dest);
    let absolute = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let mut existing = absolute.clone();
    let mut remainder: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|s| s.to_os_string()) else {
            break;
        };
        if !existing.pop() {
            break;
        }
        remainder.push(name);
    }
    let mut result = std::fs::canonicalize(&existing).unwrap_or(existing);
    for component in remainder.into_iter().rev() {
        result.push(component);
    }
    result
}

fn bootstrap_op_id_scope_for_root(root: PathBuf) -> Result<BootstrapOpIdScope> {
    use sha2::{Digest, Sha256};

    let canonical = std::fs::canonicalize(&root).unwrap_or(root);
    let label = canonical.display().to_string();
    let hash_material = format!("bootstrap-repo-root\0{label}");
    let mut hasher = Sha256::new();
    hasher.update(hash_material.as_bytes());
    let digest = hex::encode(hasher.finalize());
    Ok(BootstrapOpIdScope {
        id: digest[..16.min(digest.len())].to_string(),
        label,
        hash_material,
    })
}

fn bootstrap_op_id_store_dir(scope: &BootstrapOpIdScope) -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(".heddle").join("bootstrap-op-id").join(&scope.id)
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
    /// not opt into generated op-id persistence are never read or
    /// written here.
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
    if !crate::cli::commands::command_runtime_contract(verb)
        .map(|contract| contract.persists_op_id)
        .unwrap_or(false)
    {
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

    /// Two clones with the same `--op-id` and same relative destination
    /// but launched from different working directories must hash to
    /// different bootstrap scopes, otherwise the cache replays the
    /// wrong checkout.
    #[test]
    fn clone_destination_resolves_relative_to_supplied_cwd() {
        let cwd_a = tempfile::tempdir().expect("tempdir a");
        let cwd_b = tempfile::tempdir().expect("tempdir b");

        let from_a = super::absolutize_clone_destination("./repo", cwd_a.path());
        let from_b = super::absolutize_clone_destination("./repo", cwd_b.path());

        assert_ne!(
            from_a, from_b,
            "same relative dest from different cwds must absolutize differently"
        );
        assert!(from_a.ends_with("repo"));
        assert!(from_b.ends_with("repo"));

        // Same cwd + different relative leaves also stay distinct.
        let leaf_x = super::absolutize_clone_destination("./repo-x", cwd_a.path());
        let leaf_y = super::absolutize_clone_destination("./repo-y", cwd_a.path());
        assert_ne!(leaf_x, leaf_y);
    }

    #[test]
    fn clone_destination_preserves_absolute_paths() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let resolved =
            super::absolutize_clone_destination("/var/empty/heddle-clone-target", cwd.path());
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("heddle-clone-target"));
    }

    /// Bootstrap scopes derived from the absolutized destination must
    /// also disagree, since the hasher consumes the canonical path
    /// label. Confirms the fix flows through to `BootstrapOpIdScope`.
    #[test]
    fn bootstrap_scope_for_relative_clone_dest_is_cwd_specific() {
        let cwd_a = tempfile::tempdir().expect("tempdir a");
        let cwd_b = tempfile::tempdir().expect("tempdir b");

        let dest_a = super::absolutize_clone_destination("./repo", cwd_a.path());
        let dest_b = super::absolutize_clone_destination("./repo", cwd_b.path());
        let scope_a = super::bootstrap_op_id_scope_for_root(dest_a).expect("scope a");
        let scope_b = super::bootstrap_op_id_scope_for_root(dest_b).expect("scope b");

        assert_ne!(scope_a.id, scope_b.id);
        assert_ne!(scope_a.hash_material, scope_b.hash_material);
    }
}
