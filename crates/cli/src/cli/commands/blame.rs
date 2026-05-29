// SPDX-License-Identifier: Apache-2.0
//! Blame command - show line-by-line attribution for files.

use objects::store::ObjectStore;
use std::{collections::HashMap, path::Path};

use anyhow::{Result, anyhow};
use objects::object::{
    AnnotationStatus, ChangeId, ContentHash, ContextTarget, FileProvenance, ProvenanceError, Tree,
};
use repo::Repository;
use serde::Serialize;

use super::{
    advice::RecoveryAdvice,
    history_target::{require_resolved_state, resolve_state_id},
    snapshot::ensure_current_state,
};
use crate::{
    cli::{Cli, should_output_json},
    config::UserConfig,
};

#[derive(Serialize)]
struct BlameLine {
    line_number: usize,
    content: String,
    change_id: String,
    author: String,
    timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    origins: Option<Vec<BlameOrigin>>,
}

#[derive(Serialize)]
struct BlameOutput {
    output_kind: &'static str,
    status: &'static str,
    file: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    context: Vec<ContextSnippet>,
    lines: Vec<BlameLine>,
}

#[derive(Clone, Serialize)]
struct BlameOrigin {
    change_id: String,
    author: String,
    timestamp: String,
}

#[derive(Clone, Serialize)]
struct ContextSnippet {
    annotation_id: String,
    kind: String,
    content: String,
    revision_count: usize,
}

#[derive(Clone)]
struct LineInfo {
    change_id: ChangeId,
    author: String,
    timestamp: String,
    origins: Vec<BlameOrigin>,
}

pub fn cmd_blame(cli: &Cli, file: String, state: Option<String>, show_context: bool) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    let target_state_id = if let Some(state_id) = state {
        if matches!(state_id.as_str(), "HEAD" | "@") && repo.current_state()?.is_none() {
            ensure_current_state(
                &repo,
                &UserConfig::load_default().unwrap_or_default(),
                Some(format!("Bootstrap git-overlay before blaming {}", file)),
            )?;
        }
        resolve_state_id(&repo, &state_id)?
    } else {
        ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some(format!("Bootstrap git-overlay before blaming {}", file)),
        )?
    };

    let state_obj = require_resolved_state(&repo, &target_state_id)?;

    let tree = repo
        .store()
        .get_tree(&state_obj.tree)?
        .ok_or_else(|| anyhow!("Tree not found"))?;

    let content_hash = find_file_in_tree(&repo, &tree, Path::new(&file))?;

    let blob = repo
        .store()
        .get_blob(&content_hash)?
        .ok_or_else(|| anyhow!("Blob not found"))?;

    let content = String::from_utf8_lossy(blob.content());
    let lines: Vec<&str> = content.lines().collect();

    let provenance = repo
        .get_file_provenance_for_state(&state_obj, Path::new(&file))?
        .ok_or_else(|| {
            anyhow!(
                "No provenance data for '{}' in state {}",
                file,
                target_state_id
            )
        })?;
    let line_infos = compute_blame_from_provenance(&provenance)?;
    let context = if show_context {
        collect_file_context(&repo, &state_obj, &file)?
    } else {
        Vec::new()
    };

    // Display timestamp for the state-level fallback. Prefer
    // `authored_at` (matches git blame's default) and fall back to
    // `created_at` for native heddle commits where they're always
    // the same.
    let state_display_ts = state_obj
        .authored_at
        .unwrap_or(state_obj.created_at)
        .to_rfc3339();

    if should_output_json(cli, Some(repo.config())) {
        let output_lines: Vec<BlameLine> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let info = line_infos.get(&i).cloned().unwrap_or_else(|| LineInfo {
                    change_id: target_state_id,
                    author: state_obj.attribution.to_string(),
                    timestamp: state_display_ts.clone(),
                    origins: vec![BlameOrigin {
                        change_id: target_state_id.to_string(),
                        author: state_obj.attribution.to_string(),
                        timestamp: state_display_ts.clone(),
                    }],
                });
                BlameLine {
                    line_number: i + 1,
                    content: line.to_string(),
                    change_id: info.change_id.to_string(),
                    author: info.author,
                    timestamp: info.timestamp,
                    origins: (!info.origins.is_empty()).then_some(info.origins),
                }
            })
            .collect();

        let output = BlameOutput {
            output_kind: "blame",
            status: "completed",
            file: file.clone(),
            context,
            lines: output_lines,
        };

        println!("{}", serde_json::to_string(&output)?);
    } else {
        if show_context && !context.is_empty() {
            println!("Applicable Context:");
            println!("-------------------");
            for annotation in &context {
                println!(
                    "  [{}] {} ({} rev{})",
                    annotation.kind,
                    annotation.content,
                    annotation.revision_count,
                    if annotation.revision_count == 1 {
                        ""
                    } else {
                        "s"
                    }
                );
            }
            println!();
        }
        for (i, line) in lines.iter().enumerate() {
            let info = line_infos.get(&i).cloned().unwrap_or_else(|| LineInfo {
                change_id: target_state_id,
                author: state_obj.attribution.to_string(),
                timestamp: state_display_ts.clone(),
                origins: vec![BlameOrigin {
                    change_id: target_state_id.to_string(),
                    author: state_obj.attribution.to_string(),
                    timestamp: state_display_ts.clone(),
                }],
            });
            println!(
                "{:12} {:20} {}",
                info.change_id.short(),
                fit_author(&info.author, 20),
                line
            );
        }
    }

    Ok(())
}

fn collect_file_context(
    repo: &Repository,
    state: &objects::object::State,
    file: &str,
) -> Result<Vec<ContextSnippet>> {
    let Some(context_root) = &state.context else {
        return Ok(Vec::new());
    };
    let target = ContextTarget::file(file.to_string())?;
    let Some(blob) = repo.get_context_blob(context_root, &target)? else {
        return Ok(Vec::new());
    };
    Ok(blob
        .annotations
        .iter()
        .filter(|annotation| annotation.status == AnnotationStatus::Active)
        .filter_map(|annotation| {
            annotation
                .current_revision()
                .map(|revision| ContextSnippet {
                    annotation_id: annotation.annotation_id.clone(),
                    kind: revision.kind.to_string(),
                    content: summarize_context(&revision.content),
                    revision_count: annotation.revisions.len(),
                })
        })
        .collect())
}

fn summarize_context(content: &str) -> String {
    let first_line = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    if first_line.len() <= 88 {
        first_line.to_string()
    } else {
        format!("{}...", &first_line[..85])
    }
}

fn find_file_in_tree(repo: &Repository, tree: &Tree, file: &Path) -> Result<ContentHash> {
    let Some(name) = file.iter().next().and_then(|part| part.to_str()) else {
        return Err(anyhow!(blame_file_not_found_advice(file)));
    };
    let entry = tree
        .get(name)
        .ok_or_else(|| anyhow!(blame_file_not_found_advice(file)))?;
    let mut components = file.iter();
    components.next();
    let rest = components.as_path();
    if rest.as_os_str().is_empty() {
        return Ok(entry.hash);
    }
    if !entry.is_tree() {
        return Err(anyhow!(blame_file_not_found_advice(file)));
    }
    let subtree = repo
        .store()
        .get_tree(&entry.hash)?
        .ok_or_else(|| anyhow!(blame_file_not_found_advice(file)))?;
    find_file_in_tree(repo, &subtree, rest)
}

fn blame_file_not_found_advice(file: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "blame_file_not_found",
        format!("File '{}' not found in state", file.display()),
        "Inspect the state with `heddle show`, then retry `heddle blame <path>` with a tracked file.",
        format!(
            "requested blame path '{}' does not exist in the selected Heddle state",
            file.display()
        ),
        "blame cannot attribute lines for a path that is absent from the selected state",
        "repository state, refs, and worktree files were left unchanged",
        "heddle show",
        vec!["heddle show".to_string()],
    )
}

fn compute_blame_from_provenance(provenance: &FileProvenance) -> Result<HashMap<usize, LineInfo>> {
    provenance
        .validate()
        .map_err(|error: ProvenanceError| anyhow!(error.to_string()))?;
    let line_sets = provenance
        .line_origin_set_indexes()
        .map_err(|error: ProvenanceError| anyhow!(error.to_string()))?;
    let mut infos = HashMap::new();
    for (index, set_index) in line_sets.into_iter().enumerate() {
        let origin_set = provenance
            .origin_sets
            .get(set_index as usize)
            .ok_or_else(|| anyhow!("invalid provenance origin set"))?;
        let origins: Vec<BlameOrigin> = origin_set
            .origin_indexes
            .iter()
            .map(|origin_index| {
                let origin = &provenance.origins[*origin_index as usize];
                BlameOrigin {
                    change_id: origin.state_id.to_string(),
                    author: origin.attribution.to_string(),
                    // Prefer the authoring time when we have it
                    // (imported git history) — matches git blame's
                    // default. Falls back to `created_at` (committer
                    // time) for native heddle commits where authored
                    // and committer are always the same.
                    timestamp: origin.authored_at.unwrap_or(origin.created_at).to_rfc3339(),
                }
            })
            .collect();
        let primary = provenance
            .origins
            .get(origin_set.origin_indexes[0] as usize)
            .ok_or_else(|| anyhow!("invalid provenance origin index"))?;
        infos.insert(
            index,
            LineInfo {
                change_id: primary.state_id,
                author: if origins.len() == 1 {
                    primary.attribution.to_string()
                } else {
                    format!("{} +{}", primary.attribution, origins.len() - 1)
                },
                // Same author-vs-committer preference as the per-
                // origin timestamp above: prefer authored_at when we
                // have it (imported git history), fall back to
                // created_at for native heddle commits.
                timestamp: primary
                    .authored_at
                    .unwrap_or(primary.created_at)
                    .to_rfc3339(),
                origins,
            },
        );
    }
    Ok(infos)
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

/// Fit an attribution string (`Name <email>`) into `max_len` characters
/// without losing semantic information. The default truncation cuts
/// `"Luke Thorne <the.thorne48@gmail.com>"` to `"Luke Thorne <the..."` —
/// which keeps the noise (`<the...`) and drops the signal (the actual
/// name and email host). Strategy:
///
/// 1. If the full string fits, return it.
/// 2. If the name half alone fits, drop the email entirely.
/// 3. Otherwise truncate the name with an ellipsis.
fn fit_author(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    // Parse `Name <email>`: split at the first ` <` so we keep the
    // name intact even when it contains spaces.
    if let Some(angle) = s.find(" <") {
        let name = &s[..angle];
        if name.len() <= max_len {
            return name.to_string();
        }
    }
    truncate(s, max_len)
}
