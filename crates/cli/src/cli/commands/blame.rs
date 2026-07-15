// SPDX-License-Identifier: Apache-2.0
//! Blame command - show line-by-line attribution for files.

use std::{collections::HashMap, path::Path};

use anyhow::{Result, anyhow};
use heddle_core::{fit_author as core_fit_author, summarize_context_line};
use objects::{
    object::{
        AnnotationStatus, Attribution, ContentHash, ContextTarget, FileProvenance, ProvenanceError,
        StateId, Tree,
    },
    store::ObjectStore,
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

#[derive(Clone, Serialize)]
struct PrincipalInfo {
    name: String,
    email: String,
}

#[derive(Clone, Serialize)]
struct AgentInfo {
    provider: String,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_id: Option<String>,
}

/// Split an `Attribution` into the structured `principal` / `agent`
/// shape used by `log` and `show`, so `query --attribution --output json` consumers
/// never have to string-parse `"Name <email> (via provider/model)"`.
fn attribution_parts(attribution: &Attribution) -> (PrincipalInfo, Option<AgentInfo>) {
    let principal = PrincipalInfo {
        name: attribution.principal.name.clone(),
        email: attribution.principal.email.clone(),
    };
    let agent = attribution.agent.as_ref().map(|a| AgentInfo {
        provider: a.provider.clone(),
        model: a.model.clone(),
        session_id: a.session_id.clone(),
        policy_id: a.policy_id.clone(),
    });
    (principal, agent)
}

#[derive(Serialize)]
struct BlameLine {
    line_number: usize,
    content: String,
    state_id: String,
    principal: PrincipalInfo,
    agent: Option<AgentInfo>,
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
    state_id: String,
    principal: PrincipalInfo,
    agent: Option<AgentInfo>,
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
    state_id: StateId,
    attribution: Attribution,
    /// Count of additional origins beyond the primary, used only to
    /// render the `+N` suffix in the human-readable author column.
    extra_origins: usize,
    timestamp: String,
    origins: Vec<BlameOrigin>,
}

impl LineInfo {
    /// Author string for the human-readable (non-JSON) renderer. JSON
    /// consumers use the structured `principal` / `agent` fields and
    /// never see this.
    fn author_display(&self) -> String {
        if self.extra_origins == 0 {
            self.attribution.to_string()
        } else {
            format!("{} +{}", self.attribution, self.extra_origins)
        }
    }
}

pub fn cmd_query_attribution(
    cli: &Cli,
    file: String,
    state: Option<String>,
    show_context: bool,
) -> Result<()> {
    cmd_blame_with_output_kind(cli, file, state, show_context, "query_attribution")
}

fn cmd_blame_with_output_kind(
    cli: &Cli,
    file: String,
    state: Option<String>,
    show_context: bool,
    output_kind: &'static str,
) -> Result<()> {
    let repo = cli.open_repo()?;

    if repo.capability() == repo::RepositoryCapability::GitOverlay
        && repo.current_state()?.is_none()
    {
        let revision = state.as_deref().unwrap_or("HEAD");
        if ingest::GitSource::open(repo.root())?
            .resolve_history_revision(revision)
            .is_ok()
        {
            return render_unbound_overlay_blame(cli, &repo, &file, revision, output_kind);
        }
    }

    let target_state_id = if let Some(state_id) = state {
        if matches!(state_id.as_str(), "HEAD" | "@") && repo.current_state()?.is_none() {
            ensure_current_state(
                &repo,
                &UserConfig::load_default()?,
                Some(format!("Bootstrap git-overlay before blaming {}", file)),
            )?;
        }
        resolve_state_id(&repo, &state_id)?
    } else {
        ensure_current_state(
            &repo,
            &UserConfig::load_default()?,
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
                let info = line_infos.get(&i).cloned().unwrap_or_else(|| {
                    state_fallback_line_info(target_state_id, &state_obj, &state_display_ts)
                });
                let (principal, agent) = attribution_parts(&info.attribution);
                BlameLine {
                    line_number: i + 1,
                    content: line.to_string(),
                    state_id: info.state_id.to_string(),
                    principal,
                    agent,
                    timestamp: info.timestamp,
                    origins: (!info.origins.is_empty()).then_some(info.origins),
                }
            })
            .collect();

        let output = BlameOutput {
            output_kind,
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
            let info = line_infos.get(&i).cloned().unwrap_or_else(|| {
                state_fallback_line_info(target_state_id, &state_obj, &state_display_ts)
            });
            println!(
                "{:12} {:20} {}",
                info.state_id.short(),
                fit_author(&info.author_display(), 20),
                line
            );
        }
    }

    Ok(())
}

fn render_unbound_overlay_blame(
    cli: &Cli,
    repo: &Repository,
    file: &str,
    revision: &str,
    output_kind: &'static str,
) -> Result<()> {
    let mut lines = Vec::new();
    for line in ingest::OverlayHistory::project_blame(repo.root(), revision, file)? {
        let state = &line.state;
        let (principal, agent) = attribution_parts(&state.attribution);
        lines.push(BlameLine {
            line_number: lines.len() + 1,
            content: line.content,
            state_id: state.state_id.to_string_full(),
            principal: principal.clone(),
            agent: agent.clone(),
            timestamp: state.authored_at.unwrap_or(state.created_at).to_rfc3339(),
            origins: Some(vec![BlameOrigin {
                state_id: state.state_id.to_string_full(),
                principal,
                agent,
                timestamp: state.authored_at.unwrap_or(state.created_at).to_rfc3339(),
            }]),
        });
    }
    let output = BlameOutput {
        output_kind,
        status: "completed",
        file: file.to_string(),
        context: Vec::new(),
        lines,
    };
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        for line in &output.lines {
            let author = format!("{} <{}>", line.principal.name, line.principal.email);
            println!(
                "{:12} {:20} {}",
                &line.state_id[..line.state_id.len().min(12)],
                fit_author(&author, 20),
                line.content
            );
        }
    }
    Ok(())
}

/// Per-line attribution fallback when provenance has no origin set for
/// a line (e.g. freshly bootstrapped git-overlay): attribute the whole
/// line to the selected state.
fn state_fallback_line_info(
    state_id: StateId,
    state: &objects::object::State,
    display_ts: &str,
) -> LineInfo {
    let (principal, agent) = attribution_parts(&state.attribution);
    LineInfo {
        state_id,
        attribution: state.attribution.clone(),
        extra_origins: 0,
        timestamp: display_ts.to_string(),
        origins: vec![BlameOrigin {
            state_id: state_id.to_string(),
            principal,
            agent,
            timestamp: display_ts.to_string(),
        }],
    }
}

fn collect_file_context(
    repo: &Repository,
    state: &objects::object::State,
    file: &str,
) -> Result<Vec<ContextSnippet>> {
    let Some(context_root) = repo.inherit_parent_context(state)? else {
        return Ok(Vec::new());
    };
    let target = ContextTarget::file(file.to_string())?;
    let Some(blob) = repo.get_context_blob(&context_root, &target)? else {
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
    summarize_context_line(content)
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
        return entry
            .blob_hash()
            .ok_or_else(|| anyhow!(blame_file_not_found_advice(file)));
    }
    if !entry.is_tree() {
        return Err(anyhow!(blame_file_not_found_advice(file)));
    }
    let Some(hash) = entry.tree_hash() else {
        return Err(anyhow!(blame_file_not_found_advice(file)));
    };
    let subtree = repo
        .store()
        .get_tree(&hash)?
        .ok_or_else(|| anyhow!(blame_file_not_found_advice(file)))?;
    find_file_in_tree(repo, &subtree, rest)
}

fn blame_file_not_found_advice(file: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "blame_file_not_found",
        format!("File '{}' not found in state", file.display()),
        "Inspect the state with `heddle show`, then retry `heddle query --attribution <path>` with a tracked file.",
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
                let (principal, agent) = attribution_parts(&origin.attribution);
                BlameOrigin {
                    state_id: origin.state_id.to_string(),
                    principal,
                    agent,
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
                state_id: primary.state_id,
                attribution: primary.attribution.clone(),
                extra_origins: origins.len().saturating_sub(1),
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

fn fit_author(s: &str, max_len: usize) -> String {
    core_fit_author(s, max_len)
}

#[cfg(test)]
mod tests {
    use objects::object::{Agent, Attribution, Principal, State};

    use super::*;

    fn human() -> Attribution {
        Attribution::human(Principal::new("Ada Lovelace", "ada@example.com"))
    }

    fn agentic() -> Attribution {
        Attribution::with_agent(
            Principal::new("Ada Lovelace", "ada@example.com"),
            Agent::new("anthropic", "claude-opus-4-7")
                .with_session("sess-1", "seg-1")
                .with_policy("pol-1"),
        )
    }

    #[test]
    fn attribution_parts_splits_principal_and_agent() {
        let (principal, agent) = attribution_parts(&agentic());
        assert_eq!(principal.name, "Ada Lovelace");
        assert_eq!(principal.email, "ada@example.com");
        let agent = agent.expect("agent attribution should be structured");
        assert_eq!(agent.provider, "anthropic");
        assert_eq!(agent.model, "claude-opus-4-7");
        assert_eq!(agent.session_id.as_deref(), Some("sess-1"));
        assert_eq!(agent.policy_id.as_deref(), Some("pol-1"));
    }

    #[test]
    fn attribution_parts_omits_agent_for_human_only() {
        let (principal, agent) = attribution_parts(&human());
        assert_eq!(principal.name, "Ada Lovelace");
        assert!(
            agent.is_none(),
            "human-only attribution must not synthesize an agent"
        );
    }

    #[test]
    fn agent_info_skips_none_session_and_policy() {
        // `session_id` / `policy_id` are `skip_serializing_if = None`, so a
        // bare agent serializes to just provider + model — no null keys.
        let (_, agent) = attribution_parts(&Attribution::with_agent(
            Principal::new("Ada", "ada@example.com"),
            Agent::new("openai", "gpt-5"),
        ));
        let json = serde_json::to_value(agent.unwrap()).unwrap();
        assert_eq!(json["provider"], "openai");
        assert_eq!(json["model"], "gpt-5");
        assert!(
            json.get("session_id").is_none(),
            "absent session_id must be omitted, not null"
        );
        assert!(
            json.get("policy_id").is_none(),
            "absent policy_id must be omitted, not null"
        );
    }

    #[test]
    fn author_display_appends_extra_origin_count() {
        let info = LineInfo {
            state_id: StateId::from_bytes([70; 32]),
            attribution: human(),
            extra_origins: 2,
            timestamp: "2026-01-01T00:00:00+00:00".to_string(),
            origins: Vec::new(),
        };
        // Multi-origin lines render `Name <email> +N` in the human column.
        assert_eq!(info.author_display(), "Ada Lovelace <ada@example.com> +2");
    }

    #[test]
    fn author_display_single_origin_has_no_suffix() {
        let info = LineInfo {
            state_id: StateId::from_bytes([71; 32]),
            attribution: human(),
            extra_origins: 0,
            timestamp: "2026-01-01T00:00:00+00:00".to_string(),
            origins: Vec::new(),
        };
        assert_eq!(info.author_display(), "Ada Lovelace <ada@example.com>");
    }

    #[test]
    fn state_fallback_line_info_attributes_whole_line_to_state() {
        let state_id = StateId::from_bytes([72; 32]);
        let state = State::new(ContentHash::from_bytes([7u8; 32]), vec![], agentic());
        let ts = "2026-02-03T04:05:06+00:00";

        let info = state_fallback_line_info(state_id, &state, ts);

        assert_eq!(info.state_id, state_id);
        assert_eq!(info.extra_origins, 0);
        assert_eq!(info.timestamp, ts);
        assert_eq!(info.attribution, state.attribution);
        // Exactly one synthesized origin mirroring the state, in the same
        // structured shape the JSON renderer emits.
        assert_eq!(info.origins.len(), 1);
        let origin = &info.origins[0];
        assert_eq!(origin.state_id, state_id.to_string());
        assert_eq!(origin.timestamp, ts);
        assert_eq!(origin.principal.name, "Ada Lovelace");
        assert_eq!(
            origin.agent.as_ref().map(|a| a.provider.as_str()),
            Some("anthropic")
        );
    }
}
