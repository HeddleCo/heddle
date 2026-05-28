// SPDX-License-Identifier: Apache-2.0
//! Context annotation commands.

mod context_mutate;
mod context_query;

use anyhow::{Result, anyhow};
pub use context_mutate::*;
pub use context_query::*;
use objects::object::{
    Annotation, AnnotationKind, AnnotationScope, AnnotationStatus, ContentHash, ContextTarget,
    State,
};
use refs::Head;
use repo::Repository;
use serde::Serialize;

use super::{
    advice::RecoveryAdvice, history_target::resolve_state_id as resolve_state_id_impl,
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

/// Resolve a state specifier to a [`ChangeId`], accepting short prefixes
/// (e.g. `hd-q99fkjzgjmjv`), full IDs, or ref-manager names — matching the
/// disambiguation that `heddle show` and `heddle context list --ref` already do.
pub(crate) fn resolve_state_id(repo: &Repository, spec: &str) -> Result<objects::object::ChangeId> {
    if matches!(spec, "HEAD" | "@") && repo.current_state()?.is_none() {
        ensure_current_state(
            repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before resolving HEAD context".to_string()),
        )?;
    }
    resolve_state_id_impl(repo, spec)
}

pub(crate) fn target_label(target: &ContextTarget) -> (String, String) {
    match target {
        ContextTarget::File { path } => ("file".to_string(), path.clone()),
        ContextTarget::State { change_id } => ("state".to_string(), change_id.to_string_full()),
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

pub(crate) fn build_context_state(
    repo: &Repository,
    head_state: &State,
    new_context_root: Option<ContentHash>,
    intent: String,
) -> Result<State> {
    let user_config = UserConfig::load_default()?;
    let attribution = crate::cli::commands::snapshot::resolve_attribution(repo, &user_config)?;
    let mut new_state =
        State::new(head_state.tree, vec![head_state.change_id], attribution).with_intent(intent);
    if let Some(root) = new_context_root {
        new_state = new_state.with_context(root);
    }
    if let Some(provenance) = head_state.provenance {
        new_state = new_state.with_provenance(provenance);
    }
    Ok(new_state)
}

pub(crate) fn apply_new_state(repo: &Repository, state: &State) -> Result<()> {
    repo.store().put_state(state)?;
    advance_head(repo, state)?;
    Ok(())
}

pub(crate) fn filter_annotations<'a>(
    annotations: &'a [Annotation],
    scope: Option<&str>,
    tag: Option<&str>,
    include_superseded: bool,
) -> Result<Vec<&'a Annotation>> {
    let scope_filter = if let Some(s) = scope {
        Some(parse_scope(Some(s))?)
    } else {
        None
    };

    Ok(annotations
        .iter()
        .filter(|annotation| {
            if !include_superseded && annotation.status == AnnotationStatus::Superseded {
                return false;
            }
            if let Some(ref scope) = scope_filter
                && !annotation.scope.matches(scope)
            {
                return false;
            }
            if let Some(tag) = tag {
                let Some(current) = annotation.current_revision() else {
                    return false;
                };
                if !current.tags.iter().any(|candidate| candidate == tag) {
                    return false;
                }
            }
            true
        })
        .collect())
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
    let Some((start, end)) = range else {
        return source.to_vec();
    };
    let text = std::str::from_utf8(source).unwrap_or("");
    let lines: Vec<&str> = text.lines().collect();
    let start_idx = (start as usize).saturating_sub(1);
    let end_idx = (end as usize).min(lines.len());
    if start_idx >= lines.len() {
        return Vec::new();
    }
    lines[start_idx..end_idx].join("\n").into_bytes()
}

fn advance_head(repo: &Repository, state: &State) -> Result<()> {
    let head = repo.refs().read_head()?;
    match head {
        Head::Attached { thread } => {
            repo.refs().set_thread(&thread, &state.change_id)?;
        }
        Head::Detached { .. } => {
            repo.refs().write_head(&Head::Detached {
                state: state.change_id,
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use objects::object::{Attribution, ChangeId, Principal, Tree};

    use super::*;

    /// `heddle context get --state <short>` must accept the 12-char short prefix
    /// that the rest of the CLI emits, not just the full 26-char ChangeId.
    /// Regression: previously fell through to `ChangeId::parse`, which errored
    /// with `invalid length (expected 16 bytes)` on any short prefix.
    #[test]
    fn resolve_target_accepts_short_change_id_prefix() {
        let temp = tempfile::TempDir::new().expect("create temp dir");
        let repo = Repository::init_default(temp.path()).expect("init repo");

        let tree_hash = repo.store().put_tree(&Tree::new()).expect("put tree");
        let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
        let state = State::new(tree_hash, vec![], attribution).with_change_id(ChangeId::generate());
        repo.store().put_state(&state).expect("put state");

        let full = state.change_id.to_string_full();
        let short = state.change_id.short();
        assert_ne!(full, short, "short form should differ from full form");

        let target = resolve_target(&repo, None, Some(short.clone())).expect("resolve short");
        match target {
            ContextTarget::State { change_id } => assert_eq!(change_id, state.change_id),
            other => panic!("expected state target, got {other:?}"),
        }

        let target = resolve_target(&repo, None, Some(full)).expect("resolve full");
        match target {
            ContextTarget::State { change_id } => assert_eq!(change_id, state.change_id),
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
}
