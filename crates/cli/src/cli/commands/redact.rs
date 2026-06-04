// SPDX-License-Identifier: Apache-2.0
//! `heddle redact` — declare a redaction on a blob in a state.
//!
//! Three subverbs:
//! - `apply` writes a `Redaction` record + an `OpRecord::Redact` oplog
//!   entry. The blob bytes stay on disk; only the materialize path
//!   substitutes the stub.
//! - `list` enumerates every redaction in the repo.
//! - `show` dumps a single redaction by its content-addressed id.
//!
//! Respects `--output json` via `should_output_json`.
//!
//! `--all-states` propagates the redaction across every state reachable
//! from a thread tip or marker. The walk fans out by blob hash (not
//! path) so a leaked secret is scrubbed everywhere it appears — across
//! renames, copies, and parallel branches.

use objects::store::ObjectStore;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use crypto::{Signer, load_signer, verify_payload_signature};
use objects::{
    object::{ChangeId, ContentHash, Redaction, RedactionsBlob, StateSignature},
    worktree::should_ignore,
};
use oplog::OpLogBackend;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;

use super::advice::RecoveryAdvice;
use crate::{
    cli::{
        Cli, RedactApplyArgs, RedactCommands, RedactListArgs, RedactShowArgs, RedactTrustAddArgs,
        RedactTrustCommands, RedactTrustListArgs, RedactTrustRemoveArgs, should_output_json,
    },
    config::UserConfig,
};

pub fn cmd_redact(cli: &Cli, command: RedactCommands) -> Result<()> {
    let _user = UserConfig::load_default().unwrap_or_default();
    let repo = cli.open_repo()?;
    match command {
        RedactCommands::Apply(args) => cmd_redact_apply(cli, &repo, args),
        RedactCommands::List(args) => cmd_redact_list(cli, &repo, args),
        RedactCommands::Show(args) => cmd_redact_show(cli, &repo, args),
        RedactCommands::Trust(sub) => cmd_redact_trust(cli, &repo, sub),
    }
}

#[derive(Serialize)]
struct RedactApplyOutput {
    output_kind: &'static str,
    redaction_id: String,
    blob: String,
    state: String,
    path: String,
    reason: String,
    redactor: String,
    redacted_at: String,
    all_states: bool,
    states_redacted: u32,
    /// `true` iff the redaction carries an Ed25519/P256/RSA signature
    /// over `canonical_signing_payload`. Auditors verify via
    /// `heddle redact show`.
    signed: bool,
    /// Signature algorithm (`ed25519`, `rsa`, `p256`) when `signed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    signature_algorithm: Option<String>,
    /// Hint to add the redacted path to `.heddleignore` / `.gitignore`
    /// so subsequent captures don't re-import the leaked bytes from
    /// the working tree. `None` when the path is already covered by a
    /// glob rule in either file.
    #[serde(skip_serializing_if = "Option::is_none")]
    ignore_hint: Option<IgnoreHint>,
}

fn cmd_redact_apply(cli: &Cli, repo: &Repository, args: RedactApplyArgs) -> Result<()> {
    let state = resolve_state(repo, &args.state)?;
    let principal = repo
        .get_principal()
        .with_context(|| "resolve current principal")?;

    let blob = blob_at_path(repo, &state, &args.path)?;
    let now = Utc::now();

    // Load signer up-front so a bad `--sign-with` fails before the
    // redaction lands. Every redaction we write in this invocation
    // (primary + propagated) gets a fresh signature over its own
    // canonical payload — same operator key, different bytes.
    let signer: Option<Box<dyn Signer>> = match &args.sign_with {
        Some(path) => Some(
            load_signer(path, args.sign_algo.as_deref())
                .with_context(|| format!("load signer from '{}'", path.display()))?,
        ),
        None => None,
    };
    let signature_algorithm = signer.as_ref().map(|s| s.algorithm().to_string());

    // Always declare the redaction at the explicitly-named (state, path).
    // The redactions store is keyed by blob hash, so a single declaration
    // makes the materialize path render the stub everywhere that blob
    // surfaces — `--all-states` only adds extra audit-trail OpRecord
    // entries for each additional (state, path) the blob occupies.
    let mut primary = Redaction {
        redacted_blob: blob,
        state,
        path: args.path.clone(),
        reason: args.reason.clone(),
        redactor: principal.clone(),
        redacted_at: now,
        signature: None,
        purged_at: None,
        supersedes: None,
    };
    if let Some(signer) = &signer {
        primary.signature = Some(sign_redaction(signer.as_ref(), &primary)?);
    }
    let primary_id = repo.put_redaction(primary)?;
    let scope = repo.op_scope();
    repo.oplog()
        .record_redact(&primary_id, &blob, &state, &args.path, Some(&scope))?;

    let mut states_redacted: u32 = 1;
    let mut extra_oplog_entries: u32 = 0;
    if args.all_states {
        // Walk every reachable state, find every occurrence of the
        // leaked blob (any path), and record an oplog entry for each.
        // The redactions store key is the blob hash, so adding extra
        // (state, path) entries doesn't duplicate the stored record;
        // the audit trail does grow with each occurrence we attest to.
        let reachable = repo
            .reachable_states()
            .with_context(|| "enumerate reachable states for --all-states")?;
        for other_state in reachable {
            if other_state == state {
                continue;
            }
            let paths = repo
                .paths_to_blob_in_state(&other_state, &blob)
                .with_context(|| {
                    format!("scan state {} for blob occurrences", other_state.short())
                })?;
            if paths.is_empty() {
                continue;
            }
            states_redacted += 1;
            for path in paths {
                let mut extra = Redaction {
                    redacted_blob: blob,
                    state: other_state,
                    path: path.clone(),
                    reason: args.reason.clone(),
                    redactor: principal.clone(),
                    redacted_at: now,
                    signature: None,
                    purged_at: None,
                    supersedes: Some(primary_id),
                };
                if let Some(signer) = &signer {
                    extra.signature = Some(sign_redaction(signer.as_ref(), &extra)?);
                }
                let extra_id = repo.put_redaction(extra)?;
                repo.oplog()
                    .record_redact(&extra_id, &blob, &other_state, &path, Some(&scope))?;
                extra_oplog_entries += 1;
            }
        }
    }
    let _ = extra_oplog_entries; // surfaced via the redactions list/oplog; not in primary output

    let ignore_hint = ignore_hint_for_path(repo, &args.path)?;

    let output = RedactApplyOutput {
        output_kind: "redact_apply",
        redaction_id: primary_id.short(),
        blob: blob.short(),
        state: state.short(),
        path: args.path,
        reason: args.reason,
        redactor: format!("{} <{}>", principal.name, principal.email),
        redacted_at: now.to_rfc3339(),
        all_states: args.all_states,
        states_redacted,
        signed: signer.is_some(),
        signature_algorithm,
        ignore_hint,
    };

    emit_apply(cli, &output)
}

/// Suggestion to add a redacted path to an ignore file so it doesn't
/// get re-captured on the next `heddle capture`. Returned as `None`
/// when the path is already covered by heddle's *effective* ignore
/// set — i.e. `Repository::ignore_patterns()`, which is what the
/// capture/walker actually consults.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct IgnoreHint {
    /// Path to the ignore file we'd append to, relative to the repo
    /// root. Always `.heddleignore` — heddle's capture path doesn't
    /// read `.gitignore`, so suggesting `.gitignore` would be a
    /// false-negative on the leak-prevention guidance.
    pub ignore_file: String,
    /// Whether `.heddleignore` is already present on disk. `false`
    /// means the operator would also create the file in the same
    /// step as appending the pattern.
    pub already_exists: bool,
    /// The pattern to append. We suggest the literal redacted path
    /// here; operators who want a broader glob (`config/*.toml`) can
    /// edit before saving.
    pub suggested_pattern: String,
    /// Human-readable hint shown in text mode. Pre-formatted so the
    /// JSON consumer can surface it verbatim if they prefer.
    pub message: String,
}

/// If `path` is not yet covered by Heddle's effective ignore set,
/// return a hint pointing at the preferred ignore file for this repo:
/// `.gitignore` in Git-overlay mode, `.heddleignore` in native mode.
pub(crate) fn ignore_hint_for_path(repo: &Repository, path: &str) -> Result<Option<IgnoreHint>> {
    let patterns = repo
        .ignore_patterns()
        .with_context(|| "load .heddleignore patterns for redact-hint coverage check")?;
    if should_ignore(&PathBuf::from(path), &patterns) {
        return Ok(None);
    }

    let ignore_file = match repo.capability() {
        RepositoryCapability::GitOverlay => ".gitignore",
        RepositoryCapability::NativeHeddle => ".heddleignore",
    };
    let exists = repo.root().join(ignore_file).is_file();
    let message = if exists {
        format!(
            "hint: add `{path}` to {ignore_file} so the next `heddle capture` doesn't re-import the leaked bytes"
        )
    } else {
        format!(
            "hint: create {ignore_file} with `{path}` so the next `heddle capture` doesn't re-import the leaked bytes"
        )
    };

    Ok(Some(IgnoreHint {
        ignore_file: ignore_file.to_string(),
        already_exists: exists,
        suggested_pattern: path.to_string(),
        message,
    }))
}

/// Build a `StateSignature` over a redaction's canonical signing payload.
/// Distinct from `state_signature_from_signer`, which signs raw bytes via
/// `signer.sign`; this helper exists because `canonical_signing_payload`
/// is structurally different from a `ContentHash`.
fn sign_redaction(signer: &dyn Signer, redaction: &Redaction) -> Result<StateSignature> {
    let payload = redaction.canonical_signing_payload();
    let signature = signer
        .sign(&payload)
        .with_context(|| "sign redaction payload")?;
    Ok(StateSignature {
        algorithm: signer.algorithm().to_string(),
        public_key: hex::encode(signer.public_key()),
        signature: hex::encode(&signature),
    })
}

/// Verify a redaction's signature. Returns `Ok(true)` when verification
/// succeeds, `Ok(false)` when there's no signature, and `Err` for a
/// signature that fails to verify (tampering, wrong key, ...).
pub(crate) fn verify_redaction_signature(redaction: &Redaction) -> Result<bool> {
    let Some(signature) = &redaction.signature else {
        return Ok(false);
    };
    let payload = redaction.canonical_signing_payload();
    let public_key = hex::decode(&signature.public_key)
        .with_context(|| "decode redaction signature public key")?;
    let sig_bytes =
        hex::decode(&signature.signature).with_context(|| "decode redaction signature bytes")?;
    verify_payload_signature(&payload, &signature.algorithm, &public_key, &sig_bytes)
        .with_context(|| "verify redaction signature")?;
    Ok(true)
}

fn cmd_redact_list(cli: &Cli, repo: &Repository, _args: RedactListArgs) -> Result<()> {
    let listing = repo.list_all_redactions()?;

    #[derive(Serialize)]
    struct Row {
        redaction_id: String,
        blob: String,
        state: String,
        path: String,
        reason: String,
        redactor: String,
        redacted_at: String,
        purged: bool,
        purged_at: Option<String>,
    }
    #[derive(Serialize)]
    struct Listing {
        output_kind: &'static str,
        redactions: Vec<Row>,
        count: usize,
    }

    let mut rows: Vec<Row> = Vec::new();
    for (blob, redactions_blob) in &listing {
        for redaction in &redactions_blob.redactions {
            let id = canonical_id_for(redaction)?;
            rows.push(Row {
                redaction_id: id.short(),
                blob: blob.short(),
                state: redaction.state.short(),
                path: redaction.path.clone(),
                reason: redaction.reason.clone(),
                redactor: format!("{} <{}>", redaction.redactor.name, redaction.redactor.email),
                redacted_at: redaction.redacted_at.to_rfc3339(),
                purged: redaction.is_purged(),
                purged_at: redaction.purged_at.map(|t| t.to_rfc3339()),
            });
        }
    }

    let count = rows.len();
    let payload = Listing {
        output_kind: "redact_list",
        redactions: rows,
        count,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&payload)?);
    } else if count == 0 {
        println!("no redactions in repo");
    } else {
        println!("{} redaction(s):", count);
        for row in &payload.redactions {
            println!(
                "  {} blob={} state={} path={} {}",
                row.redaction_id,
                row.blob,
                row.state,
                row.path,
                if row.purged {
                    "[purged]"
                } else {
                    "[bytes on disk]"
                }
            );
        }
    }
    Ok(())
}

fn cmd_redact_show(cli: &Cli, repo: &Repository, args: RedactShowArgs) -> Result<()> {
    let id = resolve_redaction_id(repo, &args.redaction_id)?;
    let (blob, redaction) = repo
        .get_redaction(&id)?
        .ok_or_else(|| anyhow!("redaction '{}' not found", args.redaction_id))?;

    // Compute the signature verification status. We surface this as a
    // three-state — verified / unsigned / tampered — instead of a
    // simple boolean so auditors can distinguish "operator chose not
    // to sign" from "someone forged the file".
    let signature_status: SignatureStatus =
        match (&redaction.signature, verify_redaction_signature(&redaction)) {
            (None, _) => SignatureStatus::Unsigned,
            (Some(_), Ok(true)) => SignatureStatus::Verified,
            (Some(_), Ok(false)) => SignatureStatus::Unsigned, // unreachable in practice
            (Some(_), Err(_)) => SignatureStatus::Tampered,
        };
    let signature_algorithm = redaction.signature.as_ref().map(|s| s.algorithm.clone());

    #[derive(Serialize)]
    struct ShowOutput<'a> {
        output_kind: &'static str,
        redaction_id: String,
        blob: String,
        state: String,
        path: &'a str,
        reason: &'a str,
        redactor: String,
        redacted_at: String,
        purged_at: Option<String>,
        supersedes: Option<String>,
        signed: bool,
        signature_status: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature_algorithm: Option<String>,
        stub_preview: String,
    }
    let output = ShowOutput {
        output_kind: "redact_show",
        redaction_id: id.short(),
        blob: blob.short(),
        state: redaction.state.short(),
        path: &redaction.path,
        reason: &redaction.reason,
        redactor: format!("{} <{}>", redaction.redactor.name, redaction.redactor.email),
        redacted_at: redaction.redacted_at.to_rfc3339(),
        purged_at: redaction.purged_at.map(|t| t.to_rfc3339()),
        supersedes: redaction.supersedes.map(|h| h.short()),
        signed: redaction.signature.is_some(),
        signature_status: signature_status.label(),
        signature_algorithm,
        stub_preview: redaction.stub_text(&id),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("redaction {}", output.redaction_id);
        println!("  blob:        {}", output.blob);
        println!("  state:       {}", output.state);
        println!("  path:        {}", output.path);
        println!("  reason:      {}", output.reason);
        println!("  redactor:    {}", output.redactor);
        println!("  redacted-at: {}", output.redacted_at);
        println!(
            "  purged-at:   {}",
            output.purged_at.as_deref().unwrap_or("(bytes on disk)")
        );
        println!("  signed:      {}", output.signature_status);
        if let Some(algo) = &output.signature_algorithm {
            println!("  sig-algo:    {}", algo);
        }
        if let Some(supersedes) = &output.supersedes {
            println!("  supersedes:  {}", supersedes);
        }
        println!();
        println!("stub that readers see:");
        println!("---");
        for line in output.stub_preview.lines() {
            println!("{}", line);
        }
    }
    Ok(())
}

/// Three-state signature audit result. Distinct from the upstream
/// `crypto::SignatureStatus` enum because that one is wired to states,
/// not redactions, but the verb mapping is the same.
#[derive(Copy, Clone, Debug)]
enum SignatureStatus {
    Unsigned,
    Verified,
    Tampered,
}

impl SignatureStatus {
    fn label(self) -> &'static str {
        match self {
            SignatureStatus::Unsigned => "unsigned",
            SignatureStatus::Verified => "verified",
            SignatureStatus::Tampered => "tampered",
        }
    }
}

fn emit_apply(cli: &Cli, output: &RedactApplyOutput) -> Result<()> {
    // We open the repo above and have `&Cli` here; `Repository::config`
    // requires a borrow, so emit JSON via the local `cli` flags. The
    // helper takes `Option<&Config>`; passing `None` matches the
    // ad-hoc emitters elsewhere in the CLI (e.g., `cmd_marker`).
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(output)?);
    } else {
        println!(
            "redacted {} ({}) in {} (redaction {})",
            output.path, output.blob, output.state, output.redaction_id,
        );
        if !output.reason.is_empty() {
            println!("  reason: {}", output.reason);
        }
        if let Some(hint) = &output.ignore_hint {
            println!("  {}", hint.message);
        }
    }
    Ok(())
}

fn resolve_state(repo: &Repository, spec: &str) -> Result<ChangeId> {
    repo.resolve_state(spec)
        .with_context(|| format!("resolve state '{}'", spec))?
        .ok_or_else(|| anyhow!("state '{}' not found", spec))
}

pub(crate) fn blob_at_path(repo: &Repository, state: &ChangeId, path: &str) -> Result<ContentHash> {
    let tree = repo
        .get_tree_for_state(state)
        .with_context(|| format!("load tree for state {}", state.short()))?
        .ok_or_else(|| anyhow!("state '{}' has no tree", state.short()))?;
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err(anyhow!(RecoveryAdvice::invalid_usage(
            "redact_path_empty",
            "redact path must not be empty",
            "Pass a repository-relative path with `--path <path>`.",
            "heddle redact apply <state> --path <path>",
        )));
    }
    let hash = walk_path_to_blob(repo, &tree, &parts)?
        .ok_or_else(|| anyhow!("path '{}' not in state {}", path, state.short()))?;
    Ok(hash)
}

/// Walk a slash-split path through nested subtrees to the terminal
/// blob. Returns `Ok(None)` when any path component is missing or
/// resolves to the wrong entry type; the caller surfaces the
/// user-facing error so the message can name the original spec.
fn walk_path_to_blob(
    repo: &Repository,
    tree: &objects::object::Tree,
    parts: &[&str],
) -> Result<Option<ContentHash>> {
    if parts.is_empty() {
        return Ok(None);
    }
    let entry = match tree.get(parts[0]) {
        Some(e) => e,
        None => return Ok(None),
    };
    if parts.len() == 1 {
        if entry.is_blob() {
            return Ok(Some(entry.hash));
        }
        return Ok(None);
    }
    if !entry.is_tree() {
        return Ok(None);
    }
    let subtree = repo
        .store()
        .get_tree(&entry.hash)
        .with_context(|| format!("load subtree {}", entry.hash.short()))?
        .ok_or_else(|| anyhow!("subtree {} missing from store", entry.hash.short()))?;
    walk_path_to_blob(repo, &subtree, &parts[1..])
}

/// Resolve a `<redaction-id>` CLI argument (short or full) to its
/// canonical `ContentHash` by walking the redactions store. The
/// listing is small in practice; a flat index can be added if needed.
pub(crate) fn resolve_redaction_id(repo: &Repository, spec: &str) -> Result<ContentHash> {
    let listing = repo.list_all_redactions()?;
    let normalised = spec.trim_start_matches("hd-").to_ascii_lowercase();
    let mut candidates: Vec<ContentHash> = Vec::new();
    for (_blob, redactions_blob) in &listing {
        for redaction in &redactions_blob.redactions {
            let id = canonical_id_for(redaction)?;
            if id.short() == spec {
                return Ok(id);
            }
            let hex = hex_encode(id.as_bytes());
            if hex.starts_with(&normalised) {
                candidates.push(id);
            }
        }
    }
    match candidates.len() {
        0 => Err(anyhow!("no redaction matches '{}'", spec)),
        1 => Ok(candidates[0]),
        n => Err(anyhow!(
            "ambiguous redaction id '{}' matches {} redactions; provide a longer prefix",
            spec,
            n
        )),
    }
}

/// Mirror of
/// `crates/repo/src/repository_redaction.rs::redaction_content_hash`.
/// Crate-local because the `Repository` helper is private; same
/// canonical bytes so ids match exactly.
pub(crate) fn canonical_id_for(redaction: &Redaction) -> Result<ContentHash> {
    let single = RedactionsBlob::new(vec![redaction.clone()]);
    let bytes = single
        .encode()
        .with_context(|| "encode single-redaction for content addressing")?;
    let digest = blake3::hash(&bytes);
    Ok(ContentHash::from_bytes(*digest.as_bytes()))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{:02x}", b);
    }
    out
}

// ---------------------------------------------------------------------
// `heddle redact trust` — manage the operator trust list
//
// The trust list lives in `[redact] trusted_keys` of
// `.heddle/config.toml`. `Repository::accept_wire_redactions` consults
// it at wire-receive time; an empty list rejects every signed
// redaction (fail-closed). Operators run `trust add` after an
// out-of-band exchange of public key bytes with the redaction's
// signer — same workflow as gpg/ssh trust setup.
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct TrustEntryOutput {
    algorithm: String,
    public_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

#[derive(Serialize)]
struct TrustAddOutput {
    output_kind: &'static str,
    #[serde(flatten)]
    entry: TrustEntryOutput,
}

#[derive(Serialize)]
struct TrustListOutput {
    output_kind: &'static str,
    trusted_keys: Vec<TrustEntryOutput>,
    count: usize,
}

#[derive(Serialize)]
struct TrustRemoveOutput {
    output_kind: &'static str,
    removed: usize,
}

fn cmd_redact_trust(cli: &Cli, repo: &Repository, command: RedactTrustCommands) -> Result<()> {
    match command {
        RedactTrustCommands::Add(args) => cmd_redact_trust_add(cli, repo, args),
        RedactTrustCommands::List(args) => cmd_redact_trust_list(cli, repo, args),
        RedactTrustCommands::Remove(args) => cmd_redact_trust_remove(cli, repo, args),
    }
}

fn cmd_redact_trust_add(cli: &Cli, repo: &Repository, args: RedactTrustAddArgs) -> Result<()> {
    let (algorithm, public_key) = match (args.from_pem, args.algorithm, args.public_key) {
        (Some(pem_path), _, _) => {
            // Reuse the existing PEM loader — same code path operators
            // hit via `--sign-with`, so a PEM that works for signing
            // works for trust-add too.
            let signer = crypto::load_signer(&pem_path, None)
                .with_context(|| format!("load signer from '{}'", pem_path.display()))?;
            (
                signer.algorithm().to_string(),
                hex::encode(signer.public_key()),
            )
        }
        (None, Some(algorithm), Some(public_key)) => (algorithm, public_key),
        (None, _, _) => {
            return Err(anyhow!(RecoveryAdvice::invalid_usage(
                "redact_trust_key_source_required",
                "supply either `--from-pem <PATH>` or both `--algorithm` and `--public-key`",
                "Use `heddle redact trust add --from-pem <PATH>` or pass both raw key fields.",
                "heddle redact trust add --from-pem <PATH>",
            )));
        }
    };

    // Round-trip the config through toml::Value so we don't have to
    // re-serialize the entire typed config (which would lose
    // operator-added comments and table ordering).
    let config_path = repo.heddle_dir().join("config.toml");
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("read '{}'", config_path.display()))?;
    let mut value: toml::Value = toml::from_str(&raw).with_context(|| "parse repo config")?;
    let root = value
        .as_table_mut()
        .ok_or_else(|| anyhow!("repo config root must be a TOML table"))?;
    let redact = root
        .entry("redact".to_string())
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("[redact] section must be a table"))?;
    let trusted_keys = redact
        .entry("trusted_keys".to_string())
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow!("`trusted_keys` must be an array"))?;

    // Refuse duplicates — operators get a clear signal that the key
    // is already trusted rather than silently adding a second entry
    // (`accept_wire_redactions` would tolerate dupes but the on-disk
    // config drifts away from `cargo fmt`-clean noise-free state).
    let already_trusted = trusted_keys.iter().any(|entry| {
        entry
            .get("algorithm")
            .and_then(|v| v.as_str())
            .map(|a| a.eq_ignore_ascii_case(&algorithm))
            .unwrap_or(false)
            && entry
                .get("public_key")
                .and_then(|v| v.as_str())
                .map(|k| k.eq_ignore_ascii_case(&public_key))
                .unwrap_or(false)
    });
    if already_trusted {
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "redact_trust_key_duplicate",
            format!("key {algorithm}:{public_key} is already in the trust list"),
            "Inspect trusted redaction keys with `heddle redact trust list`.",
            format!("the trust list already contains key {algorithm}:{public_key}"),
            "adding it again would create duplicate trust metadata without changing trust",
            "repo config, trust entries, objects, refs, and worktree files were left unchanged",
            "heddle redact trust list",
            vec!["heddle redact trust list".to_string()],
        )));
    }

    let mut entry = toml::value::Table::new();
    entry.insert(
        "algorithm".to_string(),
        toml::Value::String(algorithm.clone()),
    );
    entry.insert(
        "public_key".to_string(),
        toml::Value::String(public_key.clone()),
    );
    if let Some(label) = &args.label {
        entry.insert("label".to_string(), toml::Value::String(label.clone()));
    }
    trusted_keys.push(toml::Value::Table(entry));

    let serialized = toml::to_string(&value).with_context(|| "serialize patched repo config")?;
    std::fs::write(&config_path, serialized)
        .with_context(|| format!("write '{}'", config_path.display()))?;

    let entry = TrustEntryOutput {
        algorithm,
        public_key,
        label: args.label,
    };
    let output = TrustAddOutput {
        output_kind: "redact_trust_add",
        entry,
    };
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "trusted {} key {} ({})",
            output.entry.algorithm,
            short_key(&output.entry.public_key),
            output.entry.label.as_deref().unwrap_or("unlabeled"),
        );
    }
    Ok(())
}

fn cmd_redact_trust_list(cli: &Cli, repo: &Repository, _args: RedactTrustListArgs) -> Result<()> {
    let keys: Vec<TrustEntryOutput> = repo
        .config()
        .redact
        .trusted_keys
        .iter()
        .map(|k| TrustEntryOutput {
            algorithm: k.algorithm.clone(),
            public_key: k.public_key.clone(),
            label: k.label.clone(),
        })
        .collect();
    let count = keys.len();
    let output = TrustListOutput {
        output_kind: "redact_trust_list",
        trusted_keys: keys,
        count,
    };
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else if count == 0 {
        println!("no trusted operator keys");
    } else {
        println!("{count} trusted operator key(s):");
        for k in &output.trusted_keys {
            println!(
                "  {} {} ({})",
                k.algorithm,
                short_key(&k.public_key),
                k.label.as_deref().unwrap_or("unlabeled"),
            );
        }
    }
    Ok(())
}

fn cmd_redact_trust_remove(
    cli: &Cli,
    repo: &Repository,
    args: RedactTrustRemoveArgs,
) -> Result<()> {
    let config_path = repo.heddle_dir().join("config.toml");
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("read '{}'", config_path.display()))?;
    let mut value: toml::Value = toml::from_str(&raw).with_context(|| "parse repo config")?;
    let root = value
        .as_table_mut()
        .ok_or_else(|| anyhow!("repo config root must be a TOML table"))?;
    let Some(redact) = root.get_mut("redact").and_then(|v| v.as_table_mut()) else {
        return Err(anyhow!(redact_trust_nothing_to_remove_advice(
            "redact_trust_config_missing",
            "no [redact] section in config; nothing to remove",
            &args.public_key,
        )));
    };
    let Some(trusted_keys) = redact
        .get_mut("trusted_keys")
        .and_then(|v| v.as_array_mut())
    else {
        return Err(anyhow!(redact_trust_nothing_to_remove_advice(
            "redact_trust_keys_missing",
            "no `trusted_keys` array in [redact]; nothing to remove",
            &args.public_key,
        )));
    };

    let before = trusted_keys.len();
    trusted_keys.retain(|entry| {
        entry
            .get("public_key")
            .and_then(|v| v.as_str())
            .map(|k| !k.eq_ignore_ascii_case(&args.public_key))
            .unwrap_or(true)
    });
    let removed = before - trusted_keys.len();
    if removed == 0 {
        return Err(anyhow!(redact_trust_nothing_to_remove_advice(
            "redact_trust_key_not_found",
            format!(
                "no trusted key matched `{}`; nothing removed",
                args.public_key
            ),
            &args.public_key,
        )));
    }

    let serialized = toml::to_string(&value).with_context(|| "serialize patched repo config")?;
    std::fs::write(&config_path, serialized)
        .with_context(|| format!("write '{}'", config_path.display()))?;

    if should_output_json(cli, None) {
        let output = TrustRemoveOutput {
            output_kind: "redact_trust_remove",
            removed,
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "removed {removed} trust entry/entries matching {}",
            args.public_key
        );
    }
    Ok(())
}

fn redact_trust_nothing_to_remove_advice(
    kind: &'static str,
    error: impl Into<String>,
    public_key: &str,
) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        kind,
        error,
        "Inspect trusted redaction keys with `heddle redact trust list`.",
        format!("the trust list does not contain key `{public_key}`"),
        "removing a missing key would imply a trust change that did not occur",
        "repo config, trust entries, objects, refs, and worktree files were left unchanged",
        "heddle redact trust list",
        vec!["heddle redact trust list".to_string()],
    )
}

/// Short-form display for a hex-encoded public key. Same length as
/// the redaction-id shortener: first 16 chars, which is plenty to
/// disambiguate within a single repo's trust list.
fn short_key(hex: &str) -> String {
    if hex.len() <= 16 {
        hex.to_string()
    } else {
        format!("{}…", &hex[..16])
    }
}
