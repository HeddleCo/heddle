// SPDX-License-Identifier: Apache-2.0
//! Optional git-commit coordination for `heddle merge --git-commit`.
//!
//! Closes the heddle-vs-git divergence at merge time: when the user
//! opts in, after a successful (non-preview, non-conflict) heddle merge
//! we also write a git commit on top of HEAD, staging the paths the
//! merge introduced. The default (`--git-commit` not set) is preserved
//! — heddle state advances and git is unaware.

use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use objects::{
    object::{Attribution, ChangeId},
    store::BlockingObjectStore,
};
use repo::Repository;
use serde::Serialize;
use sley::{
    CommitObject, GitObjectType, IndexWriteOptions, ObjectId as GitObjectId, RefPrecondition,
    ReferenceTarget, Repository as SleyRepository, plumbing::sley_object::EncodedObject,
};

use super::super::advice::RecoveryAdvice;
use crate::bridge::{git_core::LocalGitIdentity, git_export};

/// Outcome of `--git-commit --preview` — what *would* be committed if
/// the merge ran for real.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct GitCommitPreview {
    pub message: String,
    pub files: Vec<String>,
}

/// Outcome of a real `--git-commit` write.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct GitCommitInfo {
    pub sha: String,
    pub message: String,
}

/// Reasons the `--git-commit` request can't proceed. Surfaced via the
/// merge output's `blockers` list with `status: "blocked"`, matching
/// the schema settled by item 1.1.
#[derive(Debug)]
pub(super) struct GitCommitBlocked {
    pub blockers: Vec<String>,
}

impl std::fmt::Display for GitCommitBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "git commit blocked: {}", self.blockers.join("; "))
    }
}

impl std::error::Error for GitCommitBlocked {}

/// Validate that git is in a state where we can safely write a merge
/// commit. The merge has already enforced a clean *heddle* worktree;
/// here we additionally enforce that the only uncommitted git changes
/// are the ones the merge just produced (or, in preview mode, the ones
/// the merge would touch).
///
/// `expected_paths` is the set of paths the merge will/did write — any
/// other uncommitted git change is "unrelated" and blocks the
/// `--git-commit` flow rather than getting silently swept up.
pub(super) fn validate_git_state(
    repo: &Repository,
    expected_paths: &[String],
) -> std::result::Result<(), GitCommitBlocked> {
    let mut blockers = Vec::new();
    let repo_root = repo.root();

    if !repo_root.join(".git").exists() {
        blockers.push(format!(
            "no git repository at {} (--git-commit requires a git overlay)",
            repo_root.display()
        ));
        return Err(GitCommitBlocked { blockers });
    }

    // Detached HEAD blocks the commit — a merge commit on a detached
    // HEAD would be unreachable once HEAD moves.
    let git = match SleyRepository::discover(repo_root) {
        Ok(git) => git,
        Err(err) => {
            blockers.push(format!("failed to inspect git repository: {err}"));
            return Err(GitCommitBlocked { blockers });
        }
    };
    let attached_branch = git
        .head()
        .ok()
        .and_then(|head| head.branch_name().map(str::to_string))
        .filter(|branch| !branch.is_empty());
    if attached_branch.is_none() {
        blockers.push("git HEAD is detached (--git-commit requires an attached branch)".into());
    }

    let expected: std::collections::HashSet<&str> =
        expected_paths.iter().map(|p| p.as_str()).collect();
    let git_intent = match super::super::git_compat::git_index_intent_for_root(repo_root) {
        Ok(intent) => intent,
        Err(err) => {
            blockers.push(format!("failed to inspect git worktree status: {err}"));
            return Err(GitCommitBlocked { blockers });
        }
    };
    let unrelated = unrelated_git_index_intent_paths(&git_intent, &expected);

    if !unrelated.is_empty() {
        // Cap the rendered list — the user gets the count and a few
        // examples; the full set lives in the workspace anyway.
        // Per-path: if the path looks like common noise (`.DS_Store`,
        // `xcuserdata/...`, editor swap files), append an inline
        // `.heddleignore` hint so the user can fix the root cause in
        // one edit instead of guessing the right glob.
        let preview: Vec<String> = unrelated
            .iter()
            .take(5)
            .map(|path| {
                match super::super::heddleignore_defaults::noise_hint_for(std::path::Path::new(
                    path,
                )) {
                    Some(hint) => format!("{path} {}", hint.render_inline()),
                    None => path.clone(),
                }
            })
            .collect();
        let suffix = if unrelated.len() > preview.len() {
            format!(" (+{} more)", unrelated.len() - preview.len())
        } else {
            String::new()
        };
        blockers.push(format!(
            "{} unrelated uncommitted git change(s) outside the merge: {}{}",
            unrelated.len(),
            preview.join(", "),
            suffix
        ));
    }

    if blockers.is_empty() {
        Ok(())
    } else {
        Err(GitCommitBlocked { blockers })
    }
}

fn unrelated_git_index_intent_paths(
    intent: &super::super::git_compat::GitIndexIntent,
    expected: &std::collections::HashSet<&str>,
) -> Vec<String> {
    let mut unrelated = Vec::new();
    for path in intent.staged_paths.iter().chain(intent.extra_paths.iter()) {
        let comparison_path = path
            .strip_prefix("unstaged: ")
            .or_else(|| path.strip_prefix("untracked: "))
            .unwrap_or(path);
        if !expected.contains(comparison_path) {
            unrelated.push(path.clone());
        }
    }
    unrelated
}

/// Build the commit message. Body includes the heddle merge state ID
/// so post-merge audits can join git ↔ heddle. Trailers carry the
/// `Merge-State` change-id and a `Co-Authored-By` for the merge
/// attribution.
pub(super) fn build_commit_message(
    base_message: &str,
    merge_state_id: &str,
    attribution: &Attribution,
) -> String {
    let subject = base_message.lines().next().unwrap_or(base_message).trim();
    let mut out = String::new();
    out.push_str(subject);
    out.push_str("\n\n");
    out.push_str(&format!("Heddle merge state: {merge_state_id}\n"));
    out.push('\n');
    out.push_str(&format!("Merge-State: {merge_state_id}\n"));
    if attribution.principal.name.trim() != "Unknown"
        && attribution.principal.email.trim() != "unknown@example.com"
        && !attribution.principal.name.trim().is_empty()
        && !attribution.principal.email.trim().is_empty()
    {
        out.push_str(&format!(
            "Co-Authored-By: {} <{}>\n",
            attribution.principal.name, attribution.principal.email
        ));
    }
    out
}

/// Write a Git checkpoint commit for the landed Heddle merge state.
pub(super) fn write_git_commit(
    repo: &Repository,
    state_id: &ChangeId,
    paths: &[String],
    message: &str,
    extra_parents: &[String],
) -> Result<GitCommitInfo> {
    if paths.is_empty() {
        return Err(anyhow!(merge_git_commit_empty_advice()));
    }
    let repo_root = repo.root();
    let git = SleyRepository::discover(repo_root)
        .with_context(|| format!("failed to open Git checkout at {}", repo_root.display()))?;
    let old_head = git
        .head()
        .context("failed to resolve Git HEAD before merge --git-commit")?
        .oid
        .context("failed to resolve Git HEAD before merge --git-commit")?;
    let state = repo
        .store()
        .get_state(state_id)?
        .ok_or_else(|| anyhow!("merge state {} was not found", state_id.short()))?;
    let identity = crate::bridge::git_core::resolve_git_commit_identity(
        repo_root,
        &state.attribution.principal,
    )?;
    let tree_id = git_export::export_tree(repo, &git, &state.tree).map_err(|err| {
        anyhow!(merge_git_commit_failed_advice(
            "writing Git tree",
            err.to_string()
        ))
    })?;

    let mut parents = vec![old_head];
    for parent in extra_parents {
        let oid = parent
            .parse::<GitObjectId>()
            .with_context(|| format!("invalid extra Git parent '{parent}'"))?;
        let object = git
            .read_object(&oid)
            .with_context(|| format!("extra Git parent '{parent}' was not found"))?;
        if object.object_type != GitObjectType::Commit {
            return Err(anyhow!("extra Git parent '{parent}' is not a commit"));
        }
        if !parents.contains(&oid) {
            parents.push(oid);
        }
    }

    let seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let signature = identity.to_signature(seconds);
    let commit = CommitObject {
        tree: tree_id,
        parents,
        author: signature.to_ident_bytes(),
        committer: signature.to_ident_bytes(),
        encoding: None,
        message: message.as_bytes().to_vec(),
    };
    let commit_id = git
        .write_object(EncodedObject::new(GitObjectType::Commit, commit.write()))
        .map_err(|err| {
            anyhow!(merge_git_commit_failed_advice(
                "writing Git commit object",
                err.to_string()
            ))
        })?;

    // Keep the checkout index aligned with the committed tree. This is
    // the native equivalent of `git add <merge paths>` followed by
    // `git commit`: after HEAD moves, `git status` should be clean.
    let index = git.index_from_tree(&tree_id).map_err(|err| {
        anyhow!(merge_git_commit_failed_advice(
            "writing Git index",
            err.to_string()
        ))
    })?;
    git.write_index(
        &index,
        IndexWriteOptions {
            fsync: true,
            validate_checksum: true,
        },
    )
    .map_err(|err| {
        anyhow!(merge_git_commit_failed_advice(
            "writing Git index",
            err.to_string()
        ))
    })?;

    update_head_ref(&git, commit_id, old_head, &identity).map_err(|err| {
        anyhow!(merge_git_commit_failed_advice(
            "updating Git HEAD",
            err.to_string()
        ))
    })?;

    Ok(GitCommitInfo {
        sha: commit_id.to_string(),
        message: message.to_string(),
    })
}

fn update_head_ref(
    git: &SleyRepository,
    new_head: GitObjectId,
    old_head: GitObjectId,
    identity: &LocalGitIdentity,
) -> Result<()> {
    let seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let head = git.head().context("failed to inspect Git HEAD")?;
    let ref_name = head
        .symbolic_target
        .as_ref()
        .map(|name| name.as_str().to_string())
        .unwrap_or_else(|| "HEAD".to_string());
    let refs = git.references();
    let mut tx = refs.transaction();
    tx.update_to(
        ref_name,
        ReferenceTarget::Direct(new_head),
        RefPrecondition::MustExistAndMatch(ReferenceTarget::Direct(old_head)),
        Some(sley::plumbing::sley_refs::ReflogEntry {
            old_oid: old_head,
            new_oid: new_head,
            committer: identity.to_signature(seconds).to_ident_bytes(),
            message: b"heddle: merge --git-commit".to_vec(),
        }),
    );
    tx.commit().context("failed to update Git HEAD")?;
    Ok(())
}

fn merge_git_commit_empty_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "merge_git_commit_empty",
        "Merge produced no changed paths; refusing to write an empty Git commit",
        "Inspect repository state with `heddle status`; rerun without `--git-commit` if no Git commit is needed.",
        "the merge result has no paths to stage for Git",
        "--git-commit would create an empty Git commit that does not correspond to landed Heddle paths",
        "Heddle and Git state were left unchanged by the Git commit writer",
        "heddle status",
        vec!["heddle status".to_string()],
    )
}

fn merge_git_commit_failed_advice(stage: &'static str, detail: String) -> RecoveryAdvice {
    let detail = if detail.trim().is_empty() {
        "Git did not report a detailed error".to_string()
    } else {
        detail
    };
    RecoveryAdvice::safety_refusal(
        "merge_git_commit_failed",
        format!("{stage} failed while finalizing merge --git-commit: {detail}"),
        "Resolve the Git checkout issue, then run `heddle commit -m \"...\"`; do not rerun `heddle merge`.",
        format!("{stage} failed after Heddle merge commit coordination started"),
        "retrying the Heddle merge could duplicate or obscure the already-landed Heddle merge state",
        "the Heddle merge state is preserved; the Git commit writer did not report a completed commit",
        "heddle commit -m \"...\"",
        vec![
            "heddle commit -m \"...\"".to_string(),
            "heddle verify".to_string(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use objects::object::Principal;

    use super::*;

    #[test]
    fn build_commit_message_has_merge_state_trailer_and_coauthor() {
        let attribution = Attribution::human(Principal::new("Ada Lovelace", "ada@example.com"));
        let msg = build_commit_message("Merge thread 'feature'", "abcd1234", &attribution);
        assert!(msg.starts_with("Merge thread 'feature'\n\n"));
        assert!(msg.contains("Heddle merge state: abcd1234\n"));
        assert!(msg.contains("\nMerge-State: abcd1234\n"));
        assert!(msg.contains("Co-Authored-By: Ada Lovelace <ada@example.com>\n"));
    }

    #[test]
    fn build_commit_message_uses_only_first_subject_line() {
        let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
        let msg = build_commit_message(
            "Merge thread 'x'\n\nlonger body\nthat we drop",
            "deadbeef",
            &attribution,
        );
        // Subject line should be just the first line.
        assert!(msg.starts_with("Merge thread 'x'\n\n"));
        assert!(!msg.contains("longer body"));
    }

    #[test]
    fn merge_git_commit_empty_uses_typed_advice() {
        let advice = merge_git_commit_empty_advice();

        assert_eq!(advice.kind, "merge_git_commit_empty");
        assert_eq!(advice.primary_command, "heddle status");
        assert!(advice.error.contains("no changed paths"));
        assert!(advice.would_change.contains("empty Git commit"));
    }

    #[test]
    fn merge_git_commit_failure_uses_typed_advice() {
        let advice =
            merge_git_commit_failed_advice("writing Git index", "index locked".to_string());

        assert_eq!(advice.kind, "merge_git_commit_failed");
        assert!(advice.error.contains("writing Git index failed"));
        assert!(advice.error.contains("index locked"));
        assert!(advice.primary_command.contains("heddle commit"));
        assert!(advice.preserved.contains("Heddle merge state is preserved"));
    }
}
