// SPDX-License-Identifier: Apache-2.0
//! Context annotation commands.

mod context_mutate;
mod context_query;

use anyhow::{Result, anyhow};
pub use context_mutate::*;
pub use context_query::*;
use objects::{
    error::HeddleError,
    object::{
        Annotation, AnnotationKind, AnnotationScope, AnnotationStatus, ContentHash, ContextTarget,
        State, StateAttachment, StateAttachmentBody, Tree,
    },
    store::ObjectStore,
};
use repo::{Repository, ResolvePolicy, StateAttachmentKind};
use serde::Serialize;

use super::{
    advice::RecoveryAdvice, history_target::resolve_state_id_with_policy,
    snapshot::ensure_current_state,
};
use crate::{
    cli::{Cli, should_output_json},
    config::UserConfig,
};

#[derive(Serialize)]
pub(crate) struct RevisionOutput {
    pub(crate) revision_id: String,
    pub(crate) kind: String,
    pub(crate) content: String,
    pub(crate) tags: Vec<String>,
    pub(crate) attribution: String,
    pub(crate) created_at: i64,
}

#[derive(Serialize)]
pub(crate) struct AnnotationOutput {
    pub(crate) annotation_id: String,
    pub(crate) status: String,
    pub(crate) scope: String,
    pub(crate) kind: String,
    pub(crate) content: String,
    pub(crate) tags: Vec<String>,
    pub(crate) attribution: String,
    pub(crate) created_at: i64,
    pub(crate) revision_count: usize,
    pub(crate) supersedes_annotation_id: Option<String>,
    pub(crate) supersedes_rewrite_pct: Option<u32>,
}

impl AnnotationOutput {
    pub(crate) fn from_annotation(annotation: &Annotation) -> Self {
        let current = annotation.current_revision().expect("validated annotation");
        Self {
            annotation_id: annotation.annotation_id.clone(),
            status: match annotation.status {
                AnnotationStatus::Active => "active".to_string(),
                AnnotationStatus::Superseded => "superseded".to_string(),
            },
            scope: annotation.scope.to_string(),
            kind: current.kind.to_string(),
            content: current.content.clone(),
            tags: current.tags.clone(),
            attribution: current.attribution.clone(),
            created_at: current.created_at,
            revision_count: annotation.revisions.len(),
            supersedes_annotation_id: annotation.supersedes_annotation_id.clone(),
            supersedes_rewrite_pct: annotation.supersedes_rewrite_pct,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct ContextGetOutput {
    pub(crate) output_kind: &'static str,
    pub(crate) target_kind: String,
    pub(crate) target: String,
    pub(crate) annotations: Vec<AnnotationOutput>,
}

/// A single `context list` row. Identical to [`ContextGetOutput`] minus
/// the per-row `output_kind`: the `context_list` envelope owns the
/// discriminator, so list rows must not repeat it (consumers that route
/// recursively on `output_kind` would otherwise misclassify a list row
/// as a standalone `context get` payload).
#[derive(Serialize)]
pub(crate) struct ContextListRow {
    pub(crate) target_kind: String,
    pub(crate) target: String,
    pub(crate) annotations: Vec<AnnotationOutput>,
}

#[derive(Serialize)]
pub(crate) struct AnnotationHistoryOutput {
    pub(crate) output_kind: &'static str,
    pub(crate) annotation_id: String,
    pub(crate) target_kind: String,
    pub(crate) target: String,
    pub(crate) scope: String,
    pub(crate) status: String,
    pub(crate) supersedes_annotation_id: Option<String>,
    pub(crate) supersedes_rewrite_pct: Option<u32>,
    pub(crate) revisions: Vec<RevisionOutput>,
}

pub(crate) fn read_annotation_content(
    message: Option<String>,
    file: Option<std::path::PathBuf>,
) -> Result<String> {
    match (message, file) {
        (Some(msg), _) => Ok(msg),
        (None, Some(path)) => Ok(std::fs::read_to_string(&path)?),
        (None, None) => Err(anyhow!(RecoveryAdvice::invalid_usage(
            "context_content_required",
            "Provide annotation content with -m or --file",
            "Pass `-m <text>` or `--file <path>` with annotation content.",
            "heddle context set --path <path> -m \"...\"",
        ))),
    }
}

pub(crate) fn parse_kind(input: Option<&str>) -> Result<AnnotationKind> {
    match input.unwrap_or("rationale").parse() {
        Ok(kind) => Ok(kind),
        Err(err) => Err(anyhow::anyhow!("{err}")),
    }
}

/// For symbol scopes that don't yet have `resolved_lines`, parse the file at
/// `target` with tree-sitter and stamp the resolved range. Other scopes pass
/// through unchanged. Without this, symbol annotations are immediately stale
/// (`SymbolMissing`) on first read.
pub(crate) fn resolve_scope_at_target(
    repo: &Repository,
    target: &ContextTarget,
    scope: AnnotationScope,
) -> Result<AnnotationScope> {
    let AnnotationScope::Symbol {
        name,
        resolved_lines: None,
    } = scope
    else {
        return Ok(scope);
    };
    let Some(path) = target.path() else {
        return Ok(AnnotationScope::Symbol {
            name,
            resolved_lines: None,
        });
    };
    let source_path = repo.root().join(path);
    let source = match std::fs::read(&source_path) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Ok(AnnotationScope::Symbol {
                name,
                resolved_lines: None,
            });
        }
    };
    #[cfg(feature = "semantic")]
    {
        match repo::symbol_resolver::resolve_symbol_lines(
            &source,
            std::path::Path::new(path),
            &name,
        ) {
            Ok((start, end)) => Ok(AnnotationScope::Symbol {
                name,
                resolved_lines: Some((start, end)),
            }),
            Err(_) => Ok(AnnotationScope::Symbol {
                name,
                resolved_lines: None,
            }),
        }
    }
    #[cfg(not(feature = "semantic"))]
    {
        let _ = source;
        Ok(AnnotationScope::Symbol {
            name,
            resolved_lines: None,
        })
    }
}

pub(crate) fn parse_scope(input: Option<&str>) -> Result<AnnotationScope> {
    match input {
        None | Some("file") => Ok(AnnotationScope::File),
        Some(s) if s.starts_with("symbol:") => {
            let name = s.strip_prefix("symbol:").unwrap();
            if name.is_empty() {
                return Err(anyhow!(RecoveryAdvice::invalid_usage(
                    "context_symbol_name_required",
                    "Symbol name must not be empty",
                    "Use `--scope symbol:<name>` with a non-empty symbol name.",
                    "heddle context set --path <path> --scope symbol:<name> -m \"...\"",
                )));
            }
            Ok(AnnotationScope::Symbol {
                name: name.to_string(),
                resolved_lines: None,
            })
        }
        Some(s) if s.starts_with("lines:") => {
            let range = s.strip_prefix("lines:").unwrap();
            let (start, end) = range
                .split_once('-')
                .ok_or_else(|| anyhow::anyhow!("Line range must be 'lines:<start>-<end>'"))?;
            let start: u32 = start.parse()?;
            let end: u32 = end.parse()?;
            if start > end {
                return Err(anyhow!(RecoveryAdvice::invalid_usage(
                    "context_line_range_invalid",
                    format!("Line range start ({start}) must not exceed end ({end})"),
                    "Use `--scope lines:<start>-<end>` with start less than or equal to end.",
                    "heddle context set --path <path> --scope lines:1-10 -m \"...\"",
                )));
            }
            Ok(AnnotationScope::Lines(start, end))
        }
        Some(other) => Err(anyhow!(RecoveryAdvice::invalid_usage(
            "context_scope_invalid",
            format!(
                "Invalid scope '{other}'. Use 'file', 'symbol:<name>', or 'lines:<start>-<end>'"
            ),
            "Use `--scope file`, `--scope symbol:<name>`, or `--scope lines:<start>-<end>`.",
            "heddle context set --path <path> --scope file -m \"...\"",
        ))),
    }
}

pub(crate) fn resolve_target(
    repo: &Repository,
    path: Option<String>,
    state: Option<String>,
) -> Result<ContextTarget> {
    match (path, state) {
        (Some(path), None) => Ok(ContextTarget::file(path)?),
        (None, Some(state)) => Ok(ContextTarget::state(resolve_state_id(repo, &state)?)),
        (None, None) => Err(anyhow!(RecoveryAdvice::invalid_usage(
            "context_target_required",
            "Specify either --path or --state",
            "Pass exactly one target: `--path <path>` for file context or `--state <state>` for state context.",
            "heddle context get --path <path>",
        ))),
        (Some(_), Some(_)) => Err(anyhow!(RecoveryAdvice::invalid_usage(
            "context_target_conflict",
            "--path and --state are mutually exclusive",
            "Pass exactly one target: either `--path <path>` or `--state <state>`.",
            "heddle context get --path <path>",
        ))),
    }
}

/// Resolve a state specifier to a [`StateId`], accepting short prefixes
/// (e.g. `hs-q99fkjzgjmjv123`), full IDs, or ref-manager names — matching the
/// disambiguation that `heddle show` and `heddle context list --ref` already do.
pub(crate) fn resolve_state_id(repo: &Repository, spec: &str) -> Result<objects::object::StateId> {
    let user_config = UserConfig::load_default().unwrap_or_default();
    // The bootstrap hook must return a HeddleError, but ensure_current_state's
    // failure carries CLI-level context (IO/snapshot causes) that must reach
    // the user unrecategorized. Stash the original error and swap it back in
    // below; the HeddleError is only a carrier across the typed boundary.
    let bootstrap_failure = std::cell::RefCell::new(None);
    let bootstrap = |repo: &Repository| {
        ensure_current_state(
            repo,
            &user_config,
            Some("Bootstrap git-overlay before resolving HEAD context".to_string()),
        )
        .map(|_| ())
        .map_err(|err| {
            let carrier = HeddleError::Conflict(err.to_string());
            *bootstrap_failure.borrow_mut() = Some(err);
            carrier
        })
    };
    let policy = ResolvePolicy {
        git_import_guidance: true,
        bootstrap_on_empty_head: Some(&bootstrap),
    };
    resolve_state_id_with_policy(repo, spec, policy)
        .map_err(|err| bootstrap_failure.borrow_mut().take().unwrap_or(err))
}

pub(crate) fn target_label(target: &ContextTarget) -> (String, String) {
    match target {
        ContextTarget::File { path } => ("file".to_string(), path.clone()),
        ContextTarget::State { state_id } => ("state".to_string(), state_id.to_string_full()),
    }
}

pub(crate) fn resolve_state(repo: &Repository, r#ref: Option<&str>) -> Result<State> {
    let target_id = if let Some(spec) = r#ref {
        resolve_state_id(repo, spec)?
    } else {
        ensure_current_state(
            repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before resolving context state".to_string()),
        )?
    };

    repo.store()
        .get_state(&target_id)?
        .ok_or_else(|| anyhow::anyhow!("State not found"))
}

pub(crate) fn compute_source_hash(
    repo: &Repository,
    target: &ContextTarget,
    scope: &AnnotationScope,
) -> Option<ContentHash> {
    let path = target.path()?;
    let source_path = repo.root().join(path);
    let source_bytes = std::fs::read(&source_path).ok()?;
    let scope_bytes = match scope {
        AnnotationScope::Lines(start, end) => {
            extract_scope_bytes(&source_bytes, Some((*start, *end)))
        }
        AnnotationScope::Symbol {
            resolved_lines: Some((start, end)),
            ..
        } => extract_scope_bytes(&source_bytes, Some((*start, *end))),
        _ => source_bytes,
    };
    Some(ContentHash::from_bytes(
        *blake3::hash(&scope_bytes).as_bytes(),
    ))
}

pub(crate) fn context_root_for_state(
    repo: &Repository,
    state: &State,
) -> Result<Option<ContentHash>> {
    Ok(repo
        .latest_state_attachment(&state.state_id, StateAttachmentKind::Context)?
        .and_then(|attachment| match attachment.body {
            StateAttachmentBody::Context(root) => Some(root),
            _ => None,
        }))
}

pub(crate) fn put_context_attachment(
    repo: &Repository,
    state: &State,
    new_context_root: Option<ContentHash>,
) -> Result<ContentHash> {
    let root = match new_context_root {
        Some(root) => root,
        None => repo.store().put_tree(&Tree::new())?,
    };
    let prior = repo.latest_state_attachment(&state.state_id, StateAttachmentKind::Context)?;
    let user_config = UserConfig::load_default()?;
    let attribution = crate::cli::commands::snapshot::resolve_attribution(repo, &user_config)?;
    let created_at = prior
        .as_ref()
        .map(|attachment| attachment.created_at + chrono::Duration::nanoseconds(1))
        .map_or_else(chrono::Utc::now, |minimum| minimum.max(chrono::Utc::now()));
    repo.put_state_attachment(&StateAttachment {
        state_id: state.state_id,
        body: StateAttachmentBody::Context(root),
        attribution,
        created_at,
        supersedes: prior.map(|attachment| attachment.id()),
    })?;
    Ok(root)
}

pub(crate) fn print_context_get(
    cli: &Cli,
    target: &ContextTarget,
    annotations: Vec<&Annotation>,
) -> Result<()> {
    let (target_kind, target_label) = target_label(target);
    if should_output_json(cli, None) {
        let output = ContextGetOutput {
            output_kind: "context_get",
            target_kind,
            target: target_label,
            annotations: annotations
                .into_iter()
                .map(AnnotationOutput::from_annotation)
                .collect(),
        };
        println!("{}", serde_json::to_string(&output)?);
    } else if annotations.is_empty() {
        println!("No annotations for {target_label}");
    } else {
        println!("{target_kind} {target_label}");
        for annotation in annotations {
            let current = annotation.current_revision().unwrap();
            println!(
                "--- [{}] {} ({}) ---",
                current.kind,
                annotation.annotation_id,
                match annotation.status {
                    AnnotationStatus::Active => "active",
                    AnnotationStatus::Superseded => "superseded",
                }
            );
            if !current.tags.is_empty() {
                println!("tags: {}", current.tags.join(", "));
            }
            println!("by: {}", current.attribution);
            println!("{}", current.content);
            println!();
        }
    }
    Ok(())
}

/// Extract bytes for a line range from source content.
/// If range is None, returns the full source.
/// Lines are 1-indexed, inclusive.
fn extract_scope_bytes(source: &[u8], range: Option<(u32, u32)>) -> Vec<u8> {
    heddle_core::extract_scope_bytes(source, range)
}

#[cfg(test)]
mod tests {
    use objects::object::Tree;

    use super::*;

    /// `heddle context get --state <short>` must accept the short prefix that
    /// the rest of the CLI emits, not just the full StateId.
    /// Regression: previously fell through to `StateId::parse`, which errored
    /// with `invalid length (expected 32 bytes)` on any short prefix.
    #[test]
    fn resolve_target_accepts_short_state_id_prefix() {
        let temp = tempfile::TempDir::new().expect("create temp dir");
        let repo = Repository::init_default(temp.path()).expect("init repo");

        let tree_hash = repo.store().put_tree(&Tree::new()).expect("put tree");
        let state = State::new(
            tree_hash,
            vec![],
            objects::object::Attribution::human(objects::object::Principal::new(
                "Test",
                "test@example.com",
            )),
        );
        repo.store().put_state(&state).expect("put state");

        let full = state.state_id.to_string_full();
        let short = state.state_id.short();
        assert_ne!(full, short, "short form should differ from full form");

        let target = resolve_target(&repo, None, Some(short.clone())).expect("resolve short");
        match target {
            ContextTarget::State { state_id } => assert_eq!(state_id, state.state_id),
            other => panic!("expected state target, got {other:?}"),
        }

        let target = resolve_target(&repo, None, Some(full)).expect("resolve full");
        match target {
            ContextTarget::State { state_id } => assert_eq!(state_id, state.state_id),
            other => panic!("expected state target, got {other:?}"),
        }
    }

    /// `resolve_scope_at_target` must stamp `resolved_lines` at annotation
    /// creation time so chips can place themselves on the right rows on
    /// first read. Regression: prior to this, symbol scopes shipped with
    /// `resolved_lines: None` and rendered as immediately stale.
    ///
    /// Gated on the `semantic` feature: the resolver delegates to
    /// `repo::symbol_resolver` which requires tree-sitter. Builds
    /// without semantic legitimately leave `resolved_lines` as `None`
    /// (the "no symbol-aware resolver" path), so this test would
    /// always fail there. The behaviour exercised here is what a
    /// release build does, and CI runs with `--features semantic`
    /// for cli tests; the default `cargo test -p cli` with
    /// `client` alone does not.
    #[cfg(feature = "semantic")]
    #[test]
    fn resolve_scope_at_target_stamps_lines_for_symbol_scope() {
        let temp = tempfile::TempDir::new().expect("create temp dir");
        let repo = Repository::init_default(temp.path()).expect("init repo");

        // Write a small TS file the resolver knows how to parse.
        let src_dir = temp.path().join("src/lib");
        std::fs::create_dir_all(&src_dir).expect("create src dir");
        let file_path = src_dir.join("db.ts");
        std::fs::write(
            &file_path,
            "export const noise = 1;\n\nexport function insert(table: string, row: object) {\n  return null;\n}\n",
        )
        .expect("write file");

        let target = ContextTarget::file("src/lib/db.ts").expect("file target");
        let scope = AnnotationScope::Symbol {
            name: "insert".to_string(),
            resolved_lines: None,
        };
        let resolved = resolve_scope_at_target(&repo, &target, scope).expect("resolve");
        match resolved {
            AnnotationScope::Symbol {
                resolved_lines: Some((start, end)),
                ..
            } => {
                assert!(start >= 1 && end >= start, "got ({start}, {end})");
                assert!(start <= 3, "expected `insert` near line 3, got {start}");
            }
            other => panic!("expected resolved symbol scope, got {other:?}"),
        }
    }

    /// Symbols that don't exist in the source must pass through unchanged
    /// (resolved_lines stays None) — `cmd_context_set` rejects empty names
    /// at parse time, so absence here is "user typo / rename".
    #[test]
    fn resolve_scope_at_target_passes_through_when_symbol_absent() {
        let temp = tempfile::TempDir::new().expect("create temp dir");
        let repo = Repository::init_default(temp.path()).expect("init repo");
        let src_dir = temp.path().join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");
        std::fs::write(src_dir.join("db.ts"), "export const a = 1;\n").expect("write");

        let target = ContextTarget::file("src/db.ts").expect("file target");
        let scope = AnnotationScope::Symbol {
            name: "definitely_missing".to_string(),
            resolved_lines: None,
        };
        let resolved = resolve_scope_at_target(&repo, &target, scope).expect("resolve");
        match resolved {
            AnnotationScope::Symbol {
                resolved_lines: None,
                ..
            } => {}
            other => panic!("expected unchanged scope, got {other:?}"),
        }
    }

    #[test]
    fn context_updates_preserve_state_and_form_attachment_history() {
        let temp = tempfile::TempDir::new().expect("create temp dir");
        let repo = Repository::init_default(temp.path()).expect("init repo");
        let head_id = repo.head().expect("read head").expect("head state");
        let head_state = repo
            .store()
            .get_state(&head_id)
            .expect("read state")
            .expect("state exists");
        let original = head_state.clone();

        let first_root = ContentHash::compute(b"first-context");
        put_context_attachment(&repo, &head_state, Some(first_root)).expect("first attachment");
        let first = repo
            .latest_state_attachment(&head_id, StateAttachmentKind::Context)
            .expect("read first")
            .expect("first exists");

        let second_root = ContentHash::compute(b"second-context");
        put_context_attachment(&repo, &head_state, Some(second_root)).expect("second attachment");
        let second = repo
            .latest_state_attachment(&head_id, StateAttachmentKind::Context)
            .expect("read second")
            .expect("second exists");

        assert_eq!(repo.head().expect("read head"), Some(head_id));
        assert_eq!(
            repo.store()
                .get_state(&head_id)
                .expect("read state")
                .expect("state exists"),
            original
        );
        assert_eq!(second.supersedes, Some(first.id()));
        assert_eq!(
            context_root_for_state(&repo, &head_state).unwrap(),
            Some(second_root)
        );
    }
}
