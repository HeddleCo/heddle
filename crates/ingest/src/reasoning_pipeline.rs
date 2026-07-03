// SPDX-License-Identifier: Apache-2.0
//! Orchestrate the reasoning pass: per commit, match transcripts →
//! extract points → emit annotations.
//!
//! This module is the top of the heddle-ingest ladder — all the earlier
//! modules (matcher, extractor, emitter, sha-map, git-walk) click
//! together here. Callers pass in a set of candidate commits and a
//! pre-loaded list of [`Transcript`]s; the pipeline does the rest and
//! reports aggregated stats.
//!
//! # Why separate from `Importer`
//!
//! The mechanical import (`Importer::run`) translates git → Heddle with
//! no opinion about which commits deserve reasoning annotations. Making
//! reasoning a distinct post-pass means:
//!
//! - **Re-runnable independently.** Tuning keep thresholds or adding a
//!   new provider shouldn't force a full re-import.
//! - **Cheap opt-out.** Repos without transcript history still get a
//!   clean Heddle repo — they just skip this pass.
//! - **Scoped to what's here.** `Importer` takes raw backends and works
//!   against any `ObjectStore`; the pipeline takes a full `Repository`
//!   because writing annotations needs the context-tree helpers.
//!
//! # Commit selection
//!
//! The caller owns the commit-selection policy — pass the SHAs you want
//! processed. [`pipeline_default_commits`] is a convenience that pulls
//! every commit currently in a `ShaMap`; higher-level tooling (CLI,
//! hooks) can layer policies on top of that (date filters, limit-N,
//! etc.) without the pipeline caring.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

use objects::{
    object::{ContentHash, EntryType},
    store::ObjectStore,
};
use repo::Repository;
use tracing::{debug, info, warn};

use crate::{
    git_walk::GitSource,
    reasoning::ReasoningPoint,
    reasoning_emit::{ReasoningEmitStats, ReasoningEmitter},
    reasoning_extract::{HarvestParams, KeepParams, extract},
    sha_map::ShaMap,
    transcript::{MatchParams, Provider, Transcript, TranscriptMatcher},
};

/// Knobs for a single pipeline run.
#[derive(Clone, Debug)]
pub struct ReasoningPipelineParams {
    /// After matcher ranking, mine only the top N sessions per commit.
    /// Keeping this small (2–3) is a sanity cap: beyond the top few,
    /// match confidence usually collapses into weak "cwd + time" fits
    /// that produce noise, not notecards.
    pub max_sessions_per_commit: usize,
    /// Matcher-level floor. Below this, we don't even try to extract —
    /// a low-confidence match is more likely to smear annotations onto
    /// the wrong commit than to surface useful reasoning.
    pub min_match_confidence: f32,
    pub match_params: MatchParams,
    pub harvest: HarvestParams,
    pub keep: KeepParams,
    /// Write surviving annotations to the Heddle store. Dry-run callers
    /// set this false so matching/extraction/quality scoring still run,
    /// but no state is mutated.
    pub emit_annotations: bool,
    /// Maximum candidate-quality examples to retain for dry-run reports.
    pub preview_limit: usize,
}

impl Default for ReasoningPipelineParams {
    fn default() -> Self {
        Self {
            max_sessions_per_commit: 2,
            min_match_confidence: 0.40,
            match_params: MatchParams::default(),
            harvest: HarvestParams::default(),
            keep: KeepParams::default(),
            emit_annotations: true,
            preview_limit: 25,
        }
    }
}

/// Counters reported at the end of a pipeline run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReasoningPipelineStats {
    /// Commits we actually attempted to process (mapped, reachable).
    pub commits_scanned: usize,
    /// Commits where the matcher returned at least one candidate above
    /// `min_match_confidence`.
    pub commits_with_matches: usize,
    /// Distinct transcripts we ran the extractor on (across commits).
    pub sessions_mined: usize,
    /// Raw points the extractor produced, pre-dedup.
    pub points_extracted: usize,
    /// Points rejected after extraction because they looked like process
    /// narration, weakly-targeted text, or did not cohere with the commit.
    pub points_rejected_quality: usize,
    /// Duplicate points we collapsed before emission (same text + file
    /// target), typically from two matched sessions quoting the same
    /// sentence.
    pub points_deduped: usize,
    /// Commits skipped because their git tree wasn't in the ShaMap —
    /// either the commit predates the import or the map is stale.
    pub skipped_untranslated_tree: usize,
    /// Commits where `git.read_commit` or `diff_trees` failed. Counts a
    /// degraded but continueable run; see `tracing::warn` output for
    /// diagnostics.
    pub skipped_git_errors: usize,
    /// Rolled-up emitter stats (states updated, annotations written,
    /// dedupes *at the annotation layer*, etc.).
    pub emit: ReasoningEmitStats,
}

/// Compose the matcher + extractor + emitter over a batch of commits.
pub struct ReasoningPipeline<'a> {
    repo: &'a Repository,
    git: &'a GitSource,
    sha_map: &'a ShaMap,
    repo_root: PathBuf,
    transcripts: Vec<Transcript>,
    params: ReasoningPipelineParams,
    stats: ReasoningPipelineStats,
    preview: Vec<ReasoningPreview>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReasoningPreview {
    pub commit_sha: String,
    pub commit_subject: String,
    pub session_id: String,
    pub provider: Provider,
    pub target_file: String,
    pub kind: String,
    pub text: String,
    pub decision: PreviewDecision,
    pub reason: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewDecision {
    Kept,
    Rejected,
}

impl<'a> ReasoningPipeline<'a> {
    pub fn new(
        repo: &'a Repository,
        git: &'a GitSource,
        sha_map: &'a ShaMap,
        repo_root: impl Into<PathBuf>,
        transcripts: Vec<Transcript>,
    ) -> Self {
        Self {
            repo,
            git,
            sha_map,
            repo_root: repo_root.into(),
            transcripts,
            params: ReasoningPipelineParams::default(),
            stats: ReasoningPipelineStats::default(),
            preview: Vec::new(),
        }
    }

    pub fn with_params(mut self, params: ReasoningPipelineParams) -> Self {
        self.params = params;
        self
    }

    pub fn transcript_count(&self) -> usize {
        self.transcripts.len()
    }

    pub fn stats(&self) -> ReasoningPipelineStats {
        self.stats
    }

    pub fn preview(&self) -> &[ReasoningPreview] {
        &self.preview
    }

    /// Run the pipeline over the given git commit SHAs. Errors for
    /// individual commits are logged and counted rather than fatal —
    /// the pass is intended to be best-effort enrichment, not a
    /// blocking gate. Returns on the first unrecoverable IO / store
    /// error from the emitter.
    pub fn run(&mut self, git_shas: &[String]) -> crate::Result<ReasoningPipelineStats> {
        info!(
            commits = git_shas.len(),
            transcripts = self.transcripts.len(),
            "reasoning pipeline starting"
        );
        if self.transcripts.is_empty() {
            // No transcripts = nothing to match. Log and short-circuit;
            // the stats object documents "we tried and found nothing".
            warn!("reasoning pipeline has zero transcripts loaded — nothing to emit");
            self.stats.commits_scanned = git_shas.len();
            return Ok(self.stats);
        }

        // Lineage anchors: for every transcript with a known
        // `starting_commit`, precompute the set of commits in the
        // imported graph that descend from it. The matcher consults
        // this to bypass its strict 60-minute time gate when a
        // candidate commit can be traced forward from the session's
        // starting point — which is exactly what happens when a codex
        // worktree session's work lands via a squash merge days or
        // weeks later. Without this, those commits get rejected on
        // time-fit alone and never surface as matches.
        //
        // Build the child index once over the full reachable set
        // (from `git_shas` plus any transcripts' starting points so
        // those roots are present in the graph), then BFS forward
        // from each session's anchor. The whole pass is one pre-walk
        // + one BFS per session-with-anchor; cheap relative to the
        // per-commit extract loop below.
        let lineage_anchors = self.build_lineage_anchors(git_shas);
        if !lineage_anchors.is_empty() {
            info!(
                sessions_with_lineage = lineage_anchors.len(),
                "lineage anchors prepared (squash-merge survivors will match)"
            );
        }

        let matcher = TranscriptMatcher::new(&self.transcripts, self.repo_root.clone())
            .with_params(self.params.match_params.clone())
            .with_lineage_anchors(lineage_anchors);
        let mut emitter = self
            .params
            .emit_annotations
            .then(|| ReasoningEmitter::new(self.repo, self.sha_map));
        let mut preview = Vec::new();

        let mut last_progress = 0usize;
        for (idx, sha) in git_shas.iter().enumerate() {
            self.stats.commits_scanned += 1;

            let commit = match self.git.read_commit(sha) {
                Ok(c) => c,
                Err(e) => {
                    warn!(sha, error = %e, "read_commit failed, skipping");
                    self.stats.skipped_git_errors += 1;
                    continue;
                }
            };

            let changed = match self.changed_files(&commit) {
                Ok(v) => v,
                Err(PipelineFileError::UntranslatedTree(_)) => {
                    debug!(sha, "commit tree not in sha_map — skipping");
                    self.stats.skipped_untranslated_tree += 1;
                    continue;
                }
                Err(PipelineFileError::Other(e)) => {
                    warn!(sha, error = %e, "diff_trees failed, skipping");
                    self.stats.skipped_git_errors += 1;
                    continue;
                }
            };

            let ranked = matcher.score_commit(&commit, &changed);
            let eligible: Vec<_> = ranked
                .into_iter()
                .filter(|m| m.confidence >= self.params.min_match_confidence)
                .take(self.params.max_sessions_per_commit)
                .collect();

            if eligible.is_empty() {
                continue;
            }
            self.stats.commits_with_matches += 1;

            // A lossy string view of the (byte-typed) commit message, for the
            // preview text below. Computed once per commit, not per point.
            let commit_message = String::from_utf8_lossy(&commit.message);

            // Precompute the canonical repo root once per commit — we'll
            // normalize each point's target.file against it. Canonical
            // form handles macOS's /var → /private/var symlink so an
            // absolute `file_path` captured in a transcript still strips
            // cleanly.
            let canon_root =
                std::fs::canonicalize(&self.repo_root).unwrap_or_else(|_| self.repo_root.clone());

            // Mine each eligible session and pool the points. Dedup
            // across sessions by (text, file) — two transcripts often
            // repeat the same insight verbatim when one agent reads
            // the other's summary.
            let mut pooled: Vec<ReasoningPoint> = Vec::new();
            let mut seen: HashSet<(String, String)> = HashSet::new();
            for m in &eligible {
                let t = &self.transcripts[m.transcript_idx];
                debug!(
                    session = %m.session_id,
                    confidence = m.confidence,
                    provider = ?m.provider,
                    overlap = m.overlap_count,
                    "mining session"
                );

                let points = match extract(t, sha, &self.params.harvest, &self.params.keep) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(
                            session = %m.session_id,
                            error = %e,
                            "extract failed for session, skipping it"
                        );
                        continue;
                    }
                };
                self.stats.sessions_mined += 1;
                self.stats.points_extracted += points.len();

                for mut p in points {
                    // Normalize target.file → repo-relative. The
                    // extractor pulls file paths verbatim from tool-use
                    // inputs, which are typically absolute (e.g.
                    // `/Users/.../repo/src/auth.rs`). The emitter
                    // stores blobs in a `__files/<path>` tree and
                    // rejects anything with a RootDir component — so
                    // an absolute path makes it all the way to the
                    // store before failing. Fix it here, at the edge
                    // of the pipeline that owns the repo_root.
                    p.target.file = normalize_target_path(&p.target.file, &canon_root);
                    // `quality_decision` may demote the file target to
                    // state-scope (clear it) when the file isn't in
                    // this commit's diff — that's why it takes `&mut`.
                    let quality = quality_decision(&mut p, &changed);
                    push_preview(
                        &mut preview,
                        self.params.preview_limit,
                        &commit.sha,
                        &commit_message,
                        t,
                        &p,
                        quality.preview_decision(),
                        quality.reason(),
                    );
                    if !quality.keep {
                        self.stats.points_rejected_quality += 1;
                        continue;
                    }

                    let key = (p.text.clone(), p.target.file.clone());
                    if seen.insert(key) {
                        pooled.push(p);
                    } else {
                        self.stats.points_deduped += 1;
                    }
                }
            }

            if !pooled.is_empty()
                && let Some(emitter) = emitter.as_mut()
            {
                emitter.emit_for_commit(sha, &pooled)?;
            }

            // Progress ping every ~250 commits. The extractor can be
            // slow on fat sessions; silent long runs look hung.
            if idx - last_progress >= 250 {
                info!(
                    progress = idx + 1,
                    total = git_shas.len(),
                    "commits processed"
                );
                last_progress = idx;
            }
        }

        self.preview = preview;
        if let Some(emitter) = emitter {
            self.stats.emit = emitter.stats();
        }
        info!(stats = ?self.stats, "reasoning pipeline done");
        Ok(self.stats)
    }

    /// For each transcript with a `starting_commit`, find the set of
    /// candidate commits that descend from that starting commit in the
    /// imported graph. Returns a sparse map keyed by transcript index;
    /// transcripts without a known starting commit (Claude, OpenCode)
    /// or whose starting commit isn't in the import set are simply
    /// absent from the map and fall back to time-only matching.
    ///
    /// The graph we walk is bounded by `git_shas` plus the union of
    /// every transcript's `starting_commit`. We need the starting
    /// commits in the graph as roots so BFS can find them; without
    /// that, a session whose start predates `git_shas` would yield an
    /// empty descendant set even though all its descendants are
    /// scoreable candidates.
    fn build_lineage_anchors(&self, git_shas: &[String]) -> HashMap<usize, HashSet<String>> {
        // Quick exit: no transcript carries a starting_commit (the
        // common case for repos with only Claude/OpenCode sessions).
        if !self.transcripts.iter().any(|t| t.starting_commit.is_some()) {
            return HashMap::new();
        }
        // Seed the graph walk from `git_shas` plus every distinct
        // starting commit. `commits_topo` deduplicates.
        let mut seed: Vec<String> = git_shas.to_vec();
        for t in &self.transcripts {
            if let Some(s) = t.starting_commit.as_ref()
                && !seed.contains(s)
            {
                seed.push(s.clone());
            }
        }
        let commits = match self.git.commits_topo(seed) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "lineage walk failed; squash-merge sessions will fall back to time-only");
                return HashMap::new();
            }
        };
        let candidates: HashSet<String> = git_shas.iter().cloned().collect();
        let index = crate::git_walk::GitSource::child_index(&commits);

        let mut anchors: HashMap<usize, HashSet<String>> = HashMap::new();
        for (idx, t) in self.transcripts.iter().enumerate() {
            let Some(start) = t.starting_commit.as_ref() else {
                continue;
            };
            // Descendants exclude the root itself; include the start
            // commit explicitly when it's a candidate (a session that
            // started immediately before its commit lands counts).
            let mut set = index.descendants_of(start);
            if candidates.contains(start) {
                set.insert(start.clone());
            }
            // Restrict to commits in the scoring set so the matcher's
            // O(1) `set.contains(commit_sha)` query is the only thing
            // it ever needs.
            set.retain(|sha| candidates.contains(sha));
            if !set.is_empty() {
                anchors.insert(idx, set);
            }
        }
        anchors
    }

    /// Compute repo-relative changed *file* paths for a commit, working
    /// at leaf-file granularity even when a whole subdirectory was
    /// introduced or removed in the commit (the upstream `diff_trees`
    /// does not recurse into newly-added subtrees, so for a root commit
    /// it would surface `"src"` instead of `"src/auth.rs"` — wrong for
    /// our transcript matcher, which keys on file paths).
    ///
    /// Strategy: walk both trees fully into `(path → blob_hash)` maps
    /// and compare. Any path present on only one side, or with a
    /// different hash, is "changed". Root commits go through the same
    /// path with an empty `from` map.
    ///
    /// Assumes `Importer` already translated the commit tree and (if
    /// present) the parent tree into the Heddle store.
    fn changed_files(
        &self,
        commit: &crate::git_walk::CommitEntry,
    ) -> Result<Vec<String>, PipelineFileError> {
        let to_hash = self
            .sha_map
            .get_tree(&commit.tree_sha)
            .ok_or_else(|| PipelineFileError::UntranslatedTree(commit.tree_sha.clone()))?;

        let from_files = if let Some(parent) = commit.parents.first() {
            let parent_commit = self
                .git
                .read_commit(parent)
                .map_err(|e| PipelineFileError::Other(e.to_string()))?;
            let from_hash = self
                .sha_map
                .get_tree(&parent_commit.tree_sha)
                .ok_or_else(|| {
                    PipelineFileError::UntranslatedTree(parent_commit.tree_sha.clone())
                })?;
            collect_tree_files(self.repo.store(), &from_hash)
                .map_err(|e| PipelineFileError::Other(e.to_string()))?
        } else {
            BTreeMap::new()
        };

        let to_files = collect_tree_files(self.repo.store(), &to_hash)
            .map_err(|e| PipelineFileError::Other(e.to_string()))?;

        let mut changed: Vec<String> = Vec::new();
        for (path, to_blob) in &to_files {
            match from_files.get(path) {
                None => changed.push(path.clone()),
                Some(from_blob) if from_blob != to_blob => changed.push(path.clone()),
                _ => {}
            }
        }
        for path in from_files.keys() {
            if !to_files.contains_key(path) {
                changed.push(path.clone());
            }
        }
        changed.sort();
        changed.dedup();
        Ok(changed)
    }
}

/// Normalize a point's target-file path into a repo-relative form
/// that the context-blob writer will accept.
///
/// Transcript tool-use entries almost always record absolute paths
/// (that's what the editor/agent hands them). Absolute paths are
/// rejected deep inside `Repository::set_context_blob` because its
/// `split_path` helper refuses `RootDir` components. Empty strings
/// pass through unchanged — the emitter interprets them as
/// state-scope targets. Returns the normalized string.
///
/// Rules:
/// - Empty input → empty output (state-scope signal).
/// - Relative path → returned as-is (already repo-relative), unless it
///   looks like an absolute local path with the leading slash missing.
/// - Absolute path under `repo_root` (after canonicalization) →
///   stripped of the prefix.
/// - Absolute path under another checkout with the same repo directory
///   name → stripped to the suffix after that directory.
/// - Absolute path *not* under `repo_root` → returned with the
///   leading `/` stripped as a last-resort fallback; the emitter may
///   still reject it as malformed, which is acceptable — a point
///   that names a file outside the repo shouldn't silently pollute
///   whatever path happens to match after stripping.
fn normalize_target_path(raw: &str, repo_root: &Path) -> String {
    if raw.is_empty() {
        return String::new();
    }
    // Agent worktree prefixes for *relative* paths
    // (`.claude/worktrees/<slug>/...`, `.codex/worktrees/<slug>/...`).
    // These come from Claude Code tool-use inputs run inside an agent
    // worktree, where the slug component never contains the repo name —
    // so `strip_same_repo_checkout_prefix` (which matches on repo name)
    // can't help. Absolute paths under `~/.{claude,codex}/worktrees/...`
    // *do* embed the repo name and are handled by the existing
    // checkout-prefix logic below; we only short-circuit the relative
    // form here.
    if Path::new(raw).is_relative()
        && let Some(rel) = strip_agent_worktree_prefix(raw)
    {
        return rel;
    }
    let slashless_absolute = looks_like_slashless_absolute(raw);
    let owned_absolute;
    let as_path = if slashless_absolute {
        owned_absolute = PathBuf::from(format!("/{raw}"));
        owned_absolute.as_path()
    } else {
        Path::new(raw)
    };
    if as_path.is_relative() && !slashless_absolute {
        return raw.to_string();
    }
    // Try exact prefix strip first; fall back to the canonicalized
    // form so macOS's /var/folders → /private/var/folders symlink
    // doesn't defeat the match.
    let canon = std::fs::canonicalize(as_path).ok();
    for candidate in [Some(as_path.to_path_buf()), canon].into_iter().flatten() {
        if let Ok(rel) = candidate.strip_prefix(repo_root) {
            return rel.to_string_lossy().into_owned();
        }
        if let Some(rel) = strip_same_repo_checkout_prefix(&candidate, repo_root) {
            return rel;
        }
    }
    // Last-resort: drop the leading separator. Better than handing
    // the store an absolute path and watching `split_path` reject it.
    raw.trim_start_matches('/').to_string()
}

fn looks_like_slashless_absolute(raw: &str) -> bool {
    matches!(
        raw.split('/').next(),
        Some("Users" | "private" | "var" | "tmp" | "opt" | "home")
    )
}

/// Strip a `.claude/worktrees/<slug>/` or `.codex/worktrees/<slug>/`
/// prefix from `raw`, returning what's after the slug. Tolerates a
/// leading `/Users/<user>/` (Claude/Codex worktrees can show up as
/// absolute paths on disk) plus the relative form (most common in tool
/// inputs).
///
/// The slug component itself is not validated — both stores use opaque
/// slugs (Claude: `<adj>-<noun>-<short>`, Codex: a UUID prefix), so the
/// rule is positional: the second-to-last directory before the
/// `<rest>` suffix.
///
/// Returns `None` when no such prefix is found, so the caller can fall
/// through to other normalization paths.
fn strip_agent_worktree_prefix(raw: &str) -> Option<String> {
    // Walk components and look for the pair `(".claude" | ".codex", "worktrees")`.
    // Accept either path style: `~/.claude/worktrees/<slug>/<rest>`
    // (absolute) or `.claude/worktrees/<slug>/<rest>` (relative).
    let path = Path::new(raw);
    let components: Vec<_> = path.components().collect();
    for (idx, window) in components.windows(2).enumerate() {
        let (Component::Normal(first), Component::Normal(second)) = (&window[0], &window[1]) else {
            continue;
        };
        let first = first.to_str()?;
        let second = second.to_str()?;
        let is_agent_root = matches!(first, ".claude" | ".codex") && second == "worktrees";
        if !is_agent_root {
            continue;
        }
        // After the (`.claude`/`.codex`, `worktrees`) pair, expect a
        // slug component, then the actual repo-relative tail.
        let slug_idx = idx + 2;
        let tail_idx = slug_idx + 1;
        if tail_idx >= components.len() {
            return None;
        }
        let mut tail = PathBuf::new();
        for component in &components[tail_idx..] {
            tail.push(component.as_os_str());
        }
        if tail.as_os_str().is_empty() {
            return None;
        }
        return Some(tail.to_string_lossy().into_owned());
    }
    None
}

fn strip_same_repo_checkout_prefix(path: &Path, repo_root: &Path) -> Option<String> {
    let repo_name = repo_root.file_name()?;
    let components: Vec<_> = path.components().collect();
    for (idx, component) in components.iter().enumerate().rev() {
        if let Component::Normal(name) = component
            && name == &repo_name
        {
            let mut tail = PathBuf::new();
            for component in &components[idx + 1..] {
                tail.push(component.as_os_str());
            }
            if !tail.as_os_str().is_empty() {
                return Some(tail.to_string_lossy().into_owned());
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn push_preview(
    preview: &mut Vec<ReasoningPreview>,
    preview_limit: usize,
    commit_sha: &str,
    commit_message: &str,
    transcript: &Transcript,
    point: &ReasoningPoint,
    decision: PreviewDecision,
    reason: &'static str,
) {
    if preview.len() >= preview_limit {
        return;
    }
    let subject = commit_message.lines().next().unwrap_or("").to_string();
    preview.push(ReasoningPreview {
        commit_sha: commit_sha.to_string(),
        commit_subject: subject,
        session_id: transcript.session_id.clone(),
        provider: transcript.provider,
        target_file: point.target.file.clone(),
        kind: format!("{:?}", point.kind).to_ascii_lowercase(),
        text: point.text.clone(),
        decision,
        reason,
    });
}

#[derive(Clone, Copy, Debug)]
struct QualityDecision {
    keep: bool,
    reason: &'static str,
}

impl QualityDecision {
    fn keep(reason: &'static str) -> Self {
        Self { keep: true, reason }
    }

    fn reject(reason: &'static str) -> Self {
        Self {
            keep: false,
            reason,
        }
    }

    fn preview_decision(self) -> PreviewDecision {
        if self.keep {
            PreviewDecision::Kept
        } else {
            PreviewDecision::Rejected
        }
    }

    fn reason(self) -> &'static str {
        self.reason
    }
}

fn quality_decision(point: &mut ReasoningPoint, changed_files: &[String]) -> QualityDecision {
    let lower = point.text.to_ascii_lowercase();
    if is_process_narration(&lower) {
        return QualityDecision::reject("process narration");
    }

    // Soft demote: if the point names a file the *current* commit didn't
    // change, clear the target instead of rejecting. The matcher already
    // verified session-to-commit relevance via overlap; the harvester
    // tied this prose to a file the *session* touched. But sessions
    // routinely span multiple commits (a single feature may land across
    // several), and the prose's target file may have actually changed in
    // a sibling commit, not this one. Demoting to state-scope keeps the
    // reasoning visible without falsely claiming "this sentence is about
    // src/foo.rs when the diff only touches src/bar.rs".
    if !point.target.file.is_empty() && !changed_files.iter().any(|path| path == &point.target.file)
    {
        point.target.file = String::new();
    }

    if point.target.file.is_empty() {
        if has_durable_language(&lower) {
            return QualityDecision::keep("state-scope durable language");
        }
        return QualityDecision::reject("no target and weak durable language");
    }

    let basename = Path::new(&point.target.file)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if !basename.is_empty() && lower.contains(&basename.to_ascii_lowercase()) {
        return QualityDecision::keep("mentions target basename");
    }

    if has_durable_language(&lower) {
        return QualityDecision::keep("durable language with commit-file target");
    }

    QualityDecision::reject("weak text-target evidence")
}

fn is_process_narration(lower: &str) -> bool {
    let head = lower.trim_start();
    const PREFIXES: &[&str] = &[
        "i'm rerunning ",
        "i’m rerunning ",
        "i'm running ",
        "i’m running ",
        "i'm applying ",
        "i’m applying ",
        "i'll ",
        "i’ll ",
        "i've got ",
        "i’ve got ",
        "i found ",
        "i confirmed ",
        "i'm checking ",
        "i’m checking ",
        "next i ",
        "that should ",
        "this should ",
    ];
    PREFIXES.iter().any(|prefix| head.starts_with(prefix))
}

fn has_durable_language(lower: &str) -> bool {
    let head = lower.trim_start();
    if head.starts_with("only ") && lower.contains(" when ") {
        return true;
    }
    const DURABLE: &[&str] = &[
        "only when",
        "fallback",
        "contract",
        "boundary",
        "must ",
        "must not",
        "cannot ",
        "do not ",
        "never ",
        "always ",
        "instead of",
        "rather than",
        "because ",
        "requires ",
        "depends on",
    ];
    DURABLE.iter().any(|needle| lower.contains(needle))
}

/// Walk a tree recursively, returning every leaf file with its blob
/// hash. Directory entries expand; the returned map is keyed on the
/// slash-joined repo-relative path. Missing tree objects (shouldn't
/// happen if `Importer` succeeded) propagate as store errors.
fn collect_tree_files<S: ObjectStore + ?Sized>(
    store: &S,
    root: &ContentHash,
) -> Result<BTreeMap<String, ContentHash>, anyhow::Error> {
    let mut out = BTreeMap::new();
    walk_tree(store, root, "", &mut out)?;
    Ok(out)
}

fn walk_tree<S: ObjectStore + ?Sized>(
    store: &S,
    hash: &ContentHash,
    prefix: &str,
    out: &mut BTreeMap<String, ContentHash>,
) -> Result<(), anyhow::Error> {
    let Some(tree) = store.get_tree(hash)? else {
        // Empty tree hash resolves to None; that's fine — nothing to
        // collect, caller handles it as an empty side.
        return Ok(());
    };
    for entry in tree.entries() {
        let path = if prefix.is_empty() {
            entry.name().to_string()
        } else {
            format!("{}/{}", prefix, entry.name())
        };
        match entry.entry_type() {
            EntryType::Blob => {
                if let Some(hash) = entry.blob_hash() {
                    out.insert(path, hash);
                }
            }
            EntryType::Tree => {
                if let Some(hash) = entry.tree_hash() {
                    walk_tree(store, &hash, &path, out)?;
                }
            }
            // Treat symlinks as leaf entries — their hash identifies
            // the link target. For transcript matching we only need
            // the path, not the semantics.
            EntryType::Symlink => {
                if let Some(hash) = entry.symlink_hash() {
                    out.insert(path, hash);
                }
            }
            EntryType::Gitlink => {
                // Gitlinks point at foreign git objects, not Heddle blobs,
                // so they do not participate in transcript content matching.
            }
            EntryType::Spoollink => {
                // Native child-spool edges point at a separate spool graph,
                // not Heddle blobs, so they carry no transcript content.
            }
        }
    }
    Ok(())
}

/// Narrow internal error so the pipeline can distinguish "this commit
/// isn't in the sha map" (expected — count it) from "the store
/// exploded" (unexpected — log and move on).
enum PipelineFileError {
    UntranslatedTree(String),
    Other(String),
}

/// Convenience: return every git commit SHA currently in the map.
/// Useful for the default "process all imported commits" policy.
pub fn pipeline_default_commits(map: &ShaMap) -> Vec<String> {
    let mut v = map.commit_shas();
    v.sort();
    v
}

/// Silence `unused` lint until the CLI starts using Provider here. The
/// module re-exports it (through lib.rs) so the import stays local for
/// diagnostic log lines and tests that construct fake transcripts.
#[cfg(test)]
fn _keep_provider_in_scope(_: Provider) {}
#[cfg(not(test))]
#[allow(dead_code)]
fn _keep_provider_in_scope(_: Provider) {}

// Re-export path helper for downstream docs: Path is in the signature
// of ReasoningPipeline::new via impl Into<PathBuf>.
#[allow(dead_code)]
fn _keep_path_in_scope(_: &Path) {}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, process::Command};

    use chrono::{Duration as ChronoDuration, Utc};
    use repo::Repository;
    use tempfile::TempDir;

    use super::*;
    use crate::transcript::{FileTouch, TouchKind};

    /// Write a small Claude JSONL transcript to disk that touches
    /// `src/auth.rs` and emits a load-bearing rule in an assistant
    /// message. Returns the session_id.
    fn write_rule_session(jsonl_dir: &Path, cwd: &Path) -> (PathBuf, String) {
        let session_id = "sess-rule-0000-0000-0000-000000000000".to_string();
        let path = jsonl_dir.join(format!("{session_id}.jsonl"));
        // The extractor reads assistant events and looks for keywords
        // like "never" + a target file from a nearby tool-use. Shape
        // the JSONL so one assistant message has the prescriptive
        // sentence and the next event is a Write tool use against
        // `src/auth.rs` — that ties text to target.
        let now = Utc::now();
        let t0 = now - ChronoDuration::seconds(30);
        let t1 = now - ChronoDuration::seconds(15);
        let t2 = now;
        let lines = [
            serde_json::json!({
                "type": "user",
                "timestamp": t0.to_rfc3339(),
                "cwd": cwd.to_string_lossy(),
                "sessionId": session_id,
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": t1.to_rfc3339(),
                "cwd": cwd.to_string_lossy(),
                "sessionId": session_id,
                "message": {
                    "content": [
                        {
                            "type": "text",
                            "text": "Here's the fix. Never bypass the tenant scope in parseToken — every caller assumes it runs under a tenant."
                        }
                    ]
                }
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": t2.to_rfc3339(),
                "cwd": cwd.to_string_lossy(),
                "sessionId": session_id,
                "message": {
                    "content": [
                        {
                            "type": "tool_use",
                            "name": "Write",
                            "input": {
                                "file_path": cwd.join("src/auth.rs").to_string_lossy(),
                                "content": "fn parseToken() {}"
                            }
                        }
                    ]
                }
            }),
        ];
        let body: String = lines
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, body + "\n").unwrap();
        (path, session_id)
    }

    /// Seed a tiny git repo with one commit that writes `src/auth.rs`.
    /// Returns the commit SHA.
    fn seed_repo_with_auth(path: &Path) -> String {
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git cmd");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "-q", "--initial-branch=main"]);
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(path.join("src/auth.rs"), "fn parseToken() {}\n").unwrap();
        run(&["add", "src/auth.rs"]);
        run(&["commit", "-q", "-m", "auth: initial parseToken"]);
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(path)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Load transcripts from a test-local `.claude/projects/<slug>/*.jsonl`
    /// layout, rooted at the given home dir. Matches the shape the
    /// locator expects.
    fn set_up_claude_home_with(session_file: &Path, cwd: &Path) -> TempDir {
        let home = TempDir::new().unwrap();
        // Use the real slug encoder so the fixture tracks Claude's
        // actual on-disk layout (dots + path separators both fold to
        // `-`). Hand-rolling the slug here drifts the moment Claude
        // Code's encoder changes.
        let slug = crate::transcript::locator::claude_slug_for(cwd);
        let target_dir = home.path().join(".claude").join("projects").join(slug);
        fs::create_dir_all(&target_dir).unwrap();
        fs::copy(
            session_file,
            target_dir.join(session_file.file_name().unwrap()),
        )
        .unwrap();
        home
    }

    #[test]
    fn end_to_end_writes_invariant_annotation() {
        // Build a tiny git repo + one hand-crafted Claude session that
        // talks about auth.rs with a "never X" rule. Run the mechanical
        // import to prime the sha map + store, then run the reasoning
        // pipeline and confirm the state ended up with an Invariant-kind
        // annotation (the target AnnotationKind for extractor `Rule`
        // reasoning points) scoped to src/auth.rs.
        use objects::object::{AnnotationKind, ContextTarget};

        use crate::{
            importer::import_git_into,
            transcript::{TranscriptRoots, load_all as load_transcripts},
        };

        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        let head_sha = seed_repo_with_auth(gitdir.path());

        // Write a session JSONL to a temp location, then copy into a
        // home/.claude tree the locator can crawl.
        let scratch = TempDir::new().unwrap();
        let (session_path, _sid) = write_rule_session(scratch.path(), gitdir.path());
        let home = set_up_claude_home_with(&session_path, gitdir.path());

        // Mechanical import first.
        let (_stats, map) = import_git_into(gitdir.path(), heddledir.path()).unwrap();

        // Reasoning pass.
        let repo = Repository::open(heddledir.path()).unwrap();
        let git = GitSource::open(gitdir.path()).unwrap();
        let roots = TranscriptRoots {
            claude: Some(home.path().join(".claude")),
            codex: None,
            opencode_home: None,
            codex_since: None,
        };
        let transcripts = load_transcripts(gitdir.path(), &roots);
        assert!(
            !transcripts.is_empty(),
            "transcript loader found zero sessions under {}",
            home.path().display()
        );

        let mut pipeline = ReasoningPipeline::new(&repo, &git, &map, gitdir.path(), transcripts);
        let stats = pipeline.run(std::slice::from_ref(&head_sha)).unwrap();

        assert_eq!(stats.commits_scanned, 1);
        // The hand-crafted session overlaps the commit perfectly on
        // cwd, time, and the one changed file, so it should match.
        assert_eq!(stats.commits_with_matches, 1, "stats={stats:?}");
        assert!(stats.points_extracted >= 1, "stats={stats:?}");
        assert!(stats.emit.states_updated >= 1, "stats={stats:?}");

        // Verify the annotation surface: the state for head_sha should
        // now have a context blob at __files/src/auth.rs with an
        // Invariant-kind annotation.
        let change_id = map.get_commit(&head_sha).unwrap();
        let state = repo.store().get_state(&change_id).unwrap().unwrap();
        let root = state.context.expect("context should be set");
        let blob = repo
            .get_context_blob(&root, &ContextTarget::file("src/auth.rs").unwrap())
            .unwrap()
            .expect("blob at src/auth.rs");
        assert!(!blob.annotations.is_empty());
        let kinds: Vec<_> = blob
            .annotations
            .iter()
            .filter_map(|a| a.current_revision().map(|r| r.kind))
            .collect();
        assert!(
            kinds.iter().any(|k| matches!(k, AnnotationKind::Invariant)),
            "expected at least one Invariant annotation, got {kinds:?}"
        );
    }

    #[test]
    fn empty_transcript_list_is_short_circuit() {
        // No transcripts → pipeline should record the commit as
        // scanned but write nothing.
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        let head = seed_repo_with_auth(gitdir.path());

        use crate::importer::import_git_into;
        let (_stats, map) = import_git_into(gitdir.path(), heddledir.path()).unwrap();

        let repo = Repository::open(heddledir.path()).unwrap();
        let git = GitSource::open(gitdir.path()).unwrap();

        let mut pipeline = ReasoningPipeline::new(&repo, &git, &map, gitdir.path(), Vec::new());
        let stats = pipeline.run(&[head]).unwrap();

        // `commits_scanned` is bumped in the short-circuit too so
        // operators can see the pipeline did see the commit list.
        assert_eq!(stats.commits_scanned, 1);
        assert_eq!(stats.commits_with_matches, 0);
        assert_eq!(stats.points_extracted, 0);
        assert_eq!(stats.emit.annotations_written, 0);
    }

    #[test]
    fn unmapped_sha_counts_as_untranslated_tree() {
        // A SHA we never imported — the pipeline should classify it as
        // an untranslated tree (not crash).
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_repo_with_auth(gitdir.path());

        use crate::importer::import_git_into;
        let (_stats, map) = import_git_into(gitdir.path(), heddledir.path()).unwrap();

        let repo = Repository::open(heddledir.path()).unwrap();
        let git = GitSource::open(gitdir.path()).unwrap();

        // Synthetic "forged" transcript so transcripts.is_empty() check
        // doesn't short-circuit us past the untranslated-tree branch.
        let fake = Transcript {
            provider: Provider::Claude,
            session_id: "forged".into(),
            source_path: PathBuf::from("/nonexistent.jsonl"),
            cwd: Some(gitdir.path().to_path_buf()),
            started_at: Utc::now(),
            ended_at: Utc::now(),
            turn_count: 1,
            files_touched: vec![FileTouch {
                path: gitdir.path().join("src/auth.rs"),
                timestamp: Utc::now(),
                kind: TouchKind::Write,
            }],
            starting_commit: None,
        };

        let mut pipeline = ReasoningPipeline::new(&repo, &git, &map, gitdir.path(), vec![fake]);
        // A clearly-fake 40-char SHA that isn't in the map.
        let stats = pipeline.run(&["deadbeef".repeat(5)]).unwrap();
        assert_eq!(
            stats.skipped_git_errors + stats.skipped_untranslated_tree,
            1
        );
        assert_eq!(stats.commits_with_matches, 0);
        assert_eq!(stats.emit.annotations_written, 0);
    }

    #[test]
    fn default_commits_are_sorted() {
        let mut map = ShaMap::new();
        use objects::object::ChangeId;
        map.insert_commit(&"b".repeat(40), ChangeId::generate())
            .unwrap();
        map.insert_commit(&"a".repeat(40), ChangeId::generate())
            .unwrap();
        map.insert_commit(&"c".repeat(40), ChangeId::generate())
            .unwrap();
        let got = pipeline_default_commits(&map);
        let want: Vec<String> = vec!["a".repeat(40), "b".repeat(40), "c".repeat(40)];
        assert_eq!(got, want);
    }

    #[test]
    fn normalize_target_path_strips_slashless_absolute_same_repo_path() {
        let root = Path::new("/Users/foo/.codex/worktrees/60c1/heddle");
        let got = normalize_target_path(
            "Users/foo/dev/heddle/crates/repo/src/worktree_index_storage.rs",
            root,
        );
        assert_eq!(got, "crates/repo/src/worktree_index_storage.rs");
    }

    #[test]
    fn normalize_target_path_strips_absolute_other_checkout_of_same_repo() {
        let root = Path::new("/Users/foo/.codex/worktrees/60c1/heddle");
        let got = normalize_target_path(
            "/Users/foo/dev/heddle/crates/repo/src/status_untracked_scan.rs",
            root,
        );
        assert_eq!(got, "crates/repo/src/status_untracked_scan.rs");
    }

    fn point(text: &str, file: &str) -> ReasoningPoint {
        ReasoningPoint {
            kind: objects::object::AnnotationKind::Constraint,
            text: text.to_string(),
            target: crate::reasoning::ReasoningTarget {
                file: file.to_string(),
                symbol: None,
                line_range: None,
            },
            evidence: crate::reasoning::ReasoningEvidence {
                session_id: "s".into(),
                turn_range: (0, 0),
                commit_sha: "abc".into(),
                provider: "codex".into(),
            },
            confidence: 0.8,
        }
    }

    #[test]
    fn quality_rejects_process_narration() {
        let mut p = point(
            "I’m rerunning a targeted server check because the test should lock the contract.",
            "src/auth.rs",
        );
        let decision = quality_decision(&mut p, &["src/auth.rs".into()]);
        assert!(!decision.keep);
        assert_eq!(decision.reason(), "process narration");
    }

    #[test]
    fn quality_keeps_durable_commit_file_target() {
        let mut p = point(
            "Only auto-create harness child threads when Heddle can resolve a real base state.",
            "crates/cli/src/harness/mod.rs",
        );
        let decision = quality_decision(&mut p, &["crates/cli/src/harness/mod.rs".into()]);
        assert!(decision.keep);
    }

    #[test]
    fn quality_demotes_target_outside_commit_to_state_scope() {
        // Pre-codex-merge this rejected outright. The current rule
        // demotes to state-scope: drop the file claim, keep the prose
        // if it carries durable language. This sentence has "only" +
        // "when" which counts as durable.
        let mut p = point(
            "Only attach annotations when the target belongs to the changed commit.",
            "src/other.rs",
        );
        let decision = quality_decision(&mut p, &["src/auth.rs".into()]);
        assert!(decision.keep, "decision should keep, got {decision:?}");
        assert_eq!(decision.reason(), "state-scope durable language");
        assert_eq!(p.target.file, "", "target should have been cleared");
    }

    #[test]
    fn quality_rejects_target_outside_commit_with_no_durable_language() {
        // Same demote-to-state-scope path, but this prose has no
        // durable-language keywords, so the empty-target rule takes
        // over and rejects.
        let mut p = point("Refactored the inner loop.", "src/other.rs");
        let decision = quality_decision(&mut p, &["src/auth.rs".into()]);
        assert!(!decision.keep);
        assert_eq!(decision.reason(), "no target and weak durable language");
        assert_eq!(p.target.file, "", "target should have been cleared");
    }

    #[test]
    fn strip_agent_worktree_prefix_handles_relative_claude_paths() {
        // Claude worktree slug (no `heddle` substring) was the failure
        // case the dry-run preview surfaced — pre-fix this returned the
        // path unchanged and downstream rejected it as "target not in
        // changed_files" because the matcher was looking for the suffix.
        let got = strip_agent_worktree_prefix(
            ".claude/worktrees/dreamy-bassi-d10fc9/web/src/routes/security/+page.svelte",
        );
        assert_eq!(got.as_deref(), Some("web/src/routes/security/+page.svelte"));
    }

    #[test]
    fn normalize_handles_absolute_codex_worktree_via_existing_checkout_strip() {
        // Absolute Codex worktree paths embed the repo name (`heddle`)
        // after the slug, so they're handled by the existing
        // `strip_same_repo_checkout_prefix` path that fires for
        // absolute inputs. We don't short-circuit the agent-worktree
        // helper for absolute paths because that would leave a stray
        // `heddle/` prefix on the output.
        let root = Path::new("/Users/foo/dev/heddle");
        let got =
            normalize_target_path("/Users/foo/.codex/worktrees/89d4/heddle/src/auth.rs", root);
        assert_eq!(got, "src/auth.rs");
    }

    #[test]
    fn strip_agent_worktree_prefix_returns_none_for_normal_repo_paths() {
        assert!(strip_agent_worktree_prefix("src/auth.rs").is_none());
        assert!(strip_agent_worktree_prefix("/Users/x/dev/heddle/src/auth.rs").is_none());
        assert!(strip_agent_worktree_prefix(".claude/projects/foo.jsonl").is_none());
    }
}
