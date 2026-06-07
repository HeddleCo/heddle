// SPDX-License-Identifier: Apache-2.0
//! Score transcript ↔ commit candidates.
//!
//! The matcher takes a bag of [`Transcript`]s and, for each commit,
//! ranks the transcripts by how likely they are to have produced it.
//! Higher-confidence matches feed the reasoning-point extractor; low
//! or zero-confidence matches let the importer fall back to
//! provider-hint-only attribution (the Co-Authored-By trailer already
//! parsed in `state_writer`).
//!
//! # Scoring model
//!
//! Three core signals plus an additive lineage bonus, capped at 1.0:
//!
//! | Signal              | Weight | How it's measured                      |
//! |---------------------|--------|----------------------------------------|
//! | File overlap        | 0.65   | Weighted Jaccard over touched paths vs commit paths. |
//! | Time fit            | 0.25   | 1.0 if commit time is inside session window, decays to 0 over `grace * 2` beyond it. |
//! | Provider hint match | 0.10   | 1.0 if commit's Co-Authored-By provider agrees, 0.0 if it disagrees, 0.5 if unknown. |
//! | Lineage fit (bonus) | +0.20  | Added on top when the candidate commit descends from the session's `starting_commit`. Strictly additive (capped at 1.0) so existing in-time matches don't lose ground. |
//!
//! Eligibility gate before scoring:
//!
//! - The session's cwd must be the repo root or an ancestor/descendant
//!   (worktrees live under repo root; agent jumps to a subdir still
//!   count).
//! - At least one of (a) the commit time falls within the session
//!   window + grace, or (b) the candidate commit is a forward-reachable
//!   descendant of the session's `starting_commit`. The lineage path
//!   exists for codex worktree sessions whose work landed via squash
//!   merge days or weeks after the session — the strict 60-minute
//!   time gate would otherwise drop them.
//!
//! Failing both gates → `None`. Passing the gate but scoring below
//! [`MatchParams::min_confidence`] → also `None` (returned as filtered).
//!
//! The lineage bonus is *additive* on purpose: when we shipped the
//! lineage anchor, an early version rebalanced the three core weights
//! to make room and dropped overall match counts ~4× because borderline
//! Claude/OpenCode matches that previously scraped above the threshold
//! got pushed under it. Keeping the core formula stable and adding
//! lineage as a top-up preserves baseline coverage while still letting
//! lineage-only matches (file_overlap=1, time_fit=0, lineage=1, hint=1)
//! score 1.0.
//!
//! The model is deliberately transparent: each weight and threshold is
//! a plain constant, not a learned parameter. When a mis-attribution
//! happens in the real world we want to diagnose it by reading code,
//! not by retraining anything.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Duration, Utc};

use super::types::{Provider, Transcript};
use crate::{git_walk::CommitEntry, state_writer::parse_attribution};

/// Commit files are repo-relative. To avoid cross-repo suffix collisions,
/// we only match transcript touches that are rooted inside `repo_root`, and
/// compare them after normalizing to repo-relative paths.
fn touch_matches_commit_file(
    t: &Transcript,
    repo_root: &Path,
    touch: &Path,
    commit_file: &Path,
) -> bool {
    normalize_touch_path(t, repo_root, touch)
        .map(|relative| relative == commit_file)
        .unwrap_or(false)
}

/// Knobs the matcher reads. Exposed so tests (and eventually a CLI
/// flag) can tighten/loosen them without re-exporting every constant.
#[derive(Clone, Debug)]
pub struct MatchParams {
    /// How far outside the session window a commit is still considered
    /// eligible. 10 minutes handles the common "agent said done, user
    /// hit commit shortly after" flow.
    pub time_grace: Duration,
    /// Confidence threshold for returning a match. Below this we return
    /// `None` — better to skip than mis-attribute.
    pub min_confidence: f32,
    /// Maximum candidates to return per commit.
    pub top_n: usize,
}

impl Default for MatchParams {
    fn default() -> Self {
        Self {
            // Real-world cadence: a session ends when the agent says
            // "done", then the human tests, maybe nudges one more edit,
            // then commits. 60 minutes captures that without scooping up
            // unrelated later sessions — the Jaccard gate still filters
            // false positives on file overlap.
            time_grace: Duration::minutes(60),
            min_confidence: 0.35,
            top_n: 3,
        }
    }
}

/// One scored candidate. The `transcript_idx` indexes back into the
/// matcher's transcript list — cheaper than cloning a whole `Transcript`
/// into every `Match`.
#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub transcript_idx: usize,
    pub session_id: String,
    pub provider: Provider,
    pub confidence: f32,
    /// Component breakdown, useful for debugging borderline picks.
    pub file_overlap: f32,
    pub time_fit: f32,
    /// `1.0` if the candidate commit is a forward-reachable descendant
    /// of the session's `starting_commit`, `0.0` otherwise (or when the
    /// session didn't record a starting commit / no lineage anchors are
    /// configured). The squash-merge survival signal.
    pub lineage_fit: f32,
    pub provider_hint: f32,
    /// Count of commit-paths also touched by the session — zero means
    /// the match is riding purely on cwd + time, which is a much weaker
    /// anchor.
    pub overlap_count: usize,
}

/// Drives scoring against a fixed set of transcripts.
pub struct TranscriptMatcher<'a> {
    transcripts: &'a [Transcript],
    /// The git repo root. Used to normalize `cwd` comparison — a
    /// session running from `<repo>/crates/foo` still counts as "in the
    /// repo".
    repo_root: PathBuf,
    params: MatchParams,
    /// Per-transcript-index → set of commit SHAs that descend from that
    /// transcript's `starting_commit` in the imported commit graph.
    /// Empty / missing → fall back to time-only gating.
    lineage_anchors: HashMap<usize, HashSet<String>>,
}

impl<'a> TranscriptMatcher<'a> {
    pub fn new(transcripts: &'a [Transcript], repo_root: impl Into<PathBuf>) -> Self {
        Self {
            transcripts,
            repo_root: repo_root.into(),
            params: MatchParams::default(),
            lineage_anchors: HashMap::new(),
        }
    }

    pub fn with_params(mut self, params: MatchParams) -> Self {
        self.params = params;
        self
    }

    /// Attach a lineage anchor: for transcript at index `idx`, the set
    /// of commit SHAs that descend from its `starting_commit`. Caller
    /// (typically the pipeline) precomputes these once via
    /// [`crate::git_walk::ChildIndex`] so the matcher can answer
    /// "would this session's work plausibly have flowed into this
    /// commit?" without per-pair graph walks.
    pub fn with_lineage_anchor(mut self, idx: usize, descendants: HashSet<String>) -> Self {
        self.lineage_anchors.insert(idx, descendants);
        self
    }

    /// Bulk variant for the typical pipeline use-case where every
    /// transcript with a known `starting_commit` gets its descendant
    /// set computed in one pass.
    pub fn with_lineage_anchors(mut self, anchors: HashMap<usize, HashSet<String>>) -> Self {
        self.lineage_anchors = anchors;
        self
    }

    /// Rank transcripts for a single commit. Paths in `commit_files` are
    /// repo-relative (matches `CommitEntry`'s diff shape, though we
    /// don't compute the diff here).
    pub fn score_commit(&self, commit: &CommitEntry, commit_files: &[String]) -> Vec<Match> {
        let provider_hint = provider_hint_from_commit(commit);
        let commit_time = commit.committed_at;
        // Keep commit files repo-relative: the matcher compares touches
        // by trailing-component match, not by absolute-prefix equality.
        // That way a session whose absolute paths live under a different
        // worktree (or canonicalized form of the same path) still
        // overlaps with a commit that names the same repo-relative file.
        let commit_paths: Vec<PathBuf> = commit_files.iter().map(PathBuf::from).collect();

        let mut matches: Vec<Match> = self
            .transcripts
            .iter()
            .enumerate()
            .filter_map(|(idx, t)| {
                self.score_one(
                    idx,
                    t,
                    &commit.sha,
                    commit_time,
                    &commit_paths,
                    provider_hint,
                )
            })
            .collect();

        matches.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        matches.truncate(self.params.top_n);
        matches
    }

    /// Convenience: top-1 or `None`.
    pub fn best_match(&self, commit: &CommitEntry, commit_files: &[String]) -> Option<Match> {
        self.score_commit(commit, commit_files).into_iter().next()
    }

    fn score_one(
        &self,
        idx: usize,
        t: &Transcript,
        commit_sha: &str,
        commit_time: DateTime<Utc>,
        commit_paths: &[PathBuf],
        hint: Option<Provider>,
    ) -> Option<Match> {
        // Gate 1: cwd must be in/adjacent to repo root.
        if let Some(cwd) = t.cwd.as_ref()
            && !cwd_is_repo_local(cwd, &self.repo_root)
        {
            return None;
        }
        // Gate 2: time-window OR lineage. The OR is what saves
        // squash-merge attribution: a codex session might have ended
        // weeks before its work landed via squash on main, so the time
        // window misses, but the merge commit descends from the
        // session's `starting_commit` so lineage catches it. Either
        // signal is sufficient to enter scoring.
        let in_time_window = t.contains_time(commit_time, self.params.time_grace);
        let on_lineage = self
            .lineage_anchors
            .get(&idx)
            .is_some_and(|set| set.contains(commit_sha));
        if !in_time_window && !on_lineage {
            return None;
        }

        let time_fit = score_time_fit(t, commit_time, self.params.time_grace);
        let lineage_fit = if on_lineage { 1.0 } else { 0.0 };
        let file_overlap = score_file_overlap(t, &self.repo_root, commit_paths);
        let overlap_count = count_overlap(t, &self.repo_root, commit_paths);
        let provider_hint_score = match (hint, t.provider) {
            (Some(h), p) if h == p => 1.0,
            (Some(_), _) => 0.0,
            (None, _) => 0.5,
        };

        // Three core weights unchanged from the pre-lineage formula —
        // a rebalance regressed Claude/OpenCode coverage ~4× by pushing
        // borderline matches below the threshold. Lineage is an additive
        // bonus, capped at 1.0 so it doesn't fabricate confidence beyond
        // what the other components honestly support.
        let confidence = (0.65 * file_overlap
            + 0.25 * time_fit
            + 0.10 * provider_hint_score
            + 0.20 * lineage_fit)
            .min(1.0);

        if confidence < self.params.min_confidence {
            return None;
        }

        Some(Match {
            transcript_idx: idx,
            session_id: t.session_id.clone(),
            provider: t.provider,
            confidence,
            file_overlap,
            time_fit,
            lineage_fit,
            provider_hint: provider_hint_score,
            overlap_count,
        })
    }
}

/// `true` when the session cwd is the repo root or one of its descendants.
fn cwd_is_repo_local(cwd: &Path, repo_root: &Path) -> bool {
    super::repo_matches_checkout(cwd, repo_root)
}

fn score_file_overlap(t: &Transcript, repo_root: &Path, commit_paths: &[PathBuf]) -> f32 {
    if commit_paths.is_empty() {
        // A merge commit with no file changes or a commit whose path
        // list we couldn't compute — fall back to a neutral signal.
        return 0.0;
    }
    // We want a session that tightly touched a subset of a wide commit
    // to still score high. Two asymmetric ratios:
    //
    //   precision = hit_weight / commit_paths.len()
    //       "how much of the commit did the session cover?"
    //   recall    = write_hit_weight / write_session_weight
    //       "how much of the session's *authoring* landed in the commit?"
    //
    // Take the max: either angle scoring 1.0 means "these clearly belong
    // together."
    //
    // Recall is restricted to write-kind touches (Write/Delete, not Read).
    // A read-only session whose touched files all appear in a commit is a
    // side channel, not the authoring session — if we let reads feed recall
    // the ratio cancels out (weight/weight = 1) and a bystander session
    // outranks the real author. Precision still includes reads (weighted
    // low via `TouchKind::weight`) so a heavily-reading session can still
    // contribute to the composite score, just not monopolize it.
    let mut hit_weight = 0.0f32;
    let mut write_hit_weight = 0.0f32;
    let mut write_session_weight = 0.0f32;
    let mut seen: HashSet<&PathBuf> = HashSet::new();
    for touch in &t.files_touched {
        if !seen.insert(&touch.path) {
            continue;
        }
        let w = touch.kind.weight();
        let is_author_touch = matches!(
            touch.kind,
            super::types::TouchKind::Write | super::types::TouchKind::Delete
        );
        if is_author_touch {
            write_session_weight += w;
        }
        if commit_paths
            .iter()
            .any(|cf| touch_matches_commit_file(t, repo_root, &touch.path, cf))
        {
            hit_weight += w;
            if is_author_touch {
                write_hit_weight += w;
            }
        }
    }
    let precision = (hit_weight / commit_paths.len() as f32).min(1.0);
    let recall = if write_session_weight > 0.0 {
        (write_hit_weight / write_session_weight).min(1.0)
    } else {
        0.0
    };
    precision.max(recall)
}

fn count_overlap(t: &Transcript, repo_root: &Path, commit_paths: &[PathBuf]) -> usize {
    let mut seen: HashSet<&PathBuf> = HashSet::new();
    let mut count = 0;
    for touch in &t.files_touched {
        if !seen.insert(&touch.path) {
            continue;
        }
        if commit_paths
            .iter()
            .any(|cf| touch_matches_commit_file(t, repo_root, &touch.path, cf))
        {
            count += 1;
        }
    }
    count
}

fn normalize_touch_path(t: &Transcript, repo_root: &Path, touch: &Path) -> Option<PathBuf> {
    if touch.is_absolute() {
        if let Ok(relative) = touch.strip_prefix(repo_root) {
            return Some(relative.to_path_buf());
        }
        if let Some(session_root) = t.cwd.as_deref().and_then(super::repo_workdir)
            && let Ok(relative) = touch.strip_prefix(&session_root)
        {
            return Some(relative.to_path_buf());
        }
        return None;
    }

    if let Some(cwd) = t.cwd.as_deref()
        && let Some(session_root) = super::repo_workdir(cwd)
    {
        let resolved = cwd.join(touch);
        if let Ok(relative) = resolved.strip_prefix(&session_root) {
            return Some(relative.to_path_buf());
        }
    }

    Some(strip_worktree_prefix(touch).unwrap_or(touch).to_path_buf())
}

fn strip_worktree_prefix(path: &Path) -> Option<&Path> {
    let mut components = path.components();
    let first = components.next()?;
    let second = components.next()?;
    let _slug = components.next()?;
    let first = match first {
        std::path::Component::Normal(part) => part.to_str()?,
        _ => return None,
    };
    let second = match second {
        std::path::Component::Normal(part) => part.to_str()?,
        _ => return None,
    };
    if !matches!(first, ".claude" | ".codex") || second != "worktrees" {
        return None;
    }
    Some(components.as_path())
}

fn score_time_fit(t: &Transcript, commit_time: DateTime<Utc>, grace: Duration) -> f32 {
    if commit_time >= t.started_at && commit_time <= t.ended_at {
        return 1.0;
    }
    // Outside the window but inside grace: linear decay from 1.0 at the
    // window edge to 0.0 at `grace * 2` past it. (We doubled the grace
    // here so a commit landing exactly at the grace boundary still gets
    // ~0.5 — the gate above has already ensured we're within `grace`.)
    let dist = if commit_time < t.started_at {
        t.started_at - commit_time
    } else {
        commit_time - t.ended_at
    };
    let ratio = dist.num_seconds() as f32 / (grace.num_seconds() as f32 * 2.0);
    (1.0 - ratio).clamp(0.0, 1.0)
}

/// If the commit's Co-Authored-By trailer names a provider, return it.
/// Re-uses the attribution parser the state writer already applies.
fn provider_hint_from_commit(commit: &CommitEntry) -> Option<Provider> {
    let attrib = parse_attribution(&commit.author, &String::from_utf8_lossy(&commit.message));
    let agent = attrib.agent?;
    match agent.provider.to_lowercase().as_str() {
        "anthropic" | "claude" => Some(Provider::Claude),
        "openai" | "codex" | "chatgpt" => Some(Provider::Codex),
        "opencode" | "sst" => Some(Provider::OpenCode),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        git_walk::GitSignature,
        transcript::types::{FileTouch, TouchKind},
    };

    fn commit_entry(
        sha: &str,
        when: DateTime<Utc>,
        msg: &str,
        author: (&str, &str),
    ) -> CommitEntry {
        CommitEntry {
            sha: sha.into(),
            tree_sha: "t".into(),
            parents: vec![],
            author: GitSignature {
                name: author.0.into(),
                email: author.1.into(),
                time: when,
                tz_offset: 0,
            },
            committer: GitSignature {
                name: author.0.into(),
                email: author.1.into(),
                time: when,
                tz_offset: 0,
            },
            message: msg.as_bytes().to_vec(),
            authored_at: when,
            committed_at: when,
            extra_headers: Vec::new(),
        }
    }

    fn transcript(
        provider: Provider,
        id: &str,
        cwd: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        touches: Vec<(&str, TouchKind)>,
    ) -> Transcript {
        Transcript {
            provider,
            session_id: id.into(),
            source_path: PathBuf::from(format!("/tmp/{id}.jsonl")),
            cwd: Some(PathBuf::from(cwd)),
            started_at: start,
            ended_at: end,
            turn_count: touches.len() as u32,
            files_touched: touches
                .into_iter()
                .enumerate()
                .map(|(i, (p, k))| FileTouch {
                    path: PathBuf::from(p),
                    timestamp: start + Duration::seconds(i as i64),
                    kind: k,
                })
                .collect(),
            starting_commit: None,
        }
    }

    #[test]
    fn high_overlap_inside_window_scores_strongly() {
        let start = Utc::now() - Duration::minutes(30);
        let end = Utc::now() - Duration::minutes(5);
        let when = Utc::now() - Duration::minutes(10);

        let t = transcript(
            Provider::Claude,
            "s1",
            "/repo",
            start,
            end,
            vec![
                ("/repo/a.rs", TouchKind::Write),
                ("/repo/b.rs", TouchKind::Write),
                ("/repo/c.rs", TouchKind::Read),
            ],
        );
        let commit = commit_entry(
            "abc",
            when,
            "add something\n\nCo-Authored-By: Claude <claude@anthropic.com>",
            ("Me", "me@x"),
        );
        let m = TranscriptMatcher::new(&[t], "/repo")
            .best_match(&commit, &["a.rs".into(), "b.rs".into()])
            .expect("should match");
        assert!(m.confidence > 0.8, "got {m:?}");
        assert_eq!(m.overlap_count, 2);
        assert_eq!(m.provider, Provider::Claude);
    }

    #[test]
    fn read_only_session_does_not_outrank_real_author() {
        // Regression for Codex review P1 on #32: previously, a session
        // whose touches were all Reads could score recall = 1.0 via
        // `hit_weight / session_weight` (the weight/weight ratio cancels),
        // letting a bystander session outrank the actual authoring
        // session on a big commit. Recall is now restricted to
        // Write/Delete touches, so a pure-read session loses the recall
        // shortcut and falls back to precision — which is low when the
        // commit is wide.
        let start = Utc::now() - Duration::minutes(30);
        let end = Utc::now() - Duration::minutes(5);
        let when = Utc::now() - Duration::minutes(10);

        // 10-file commit; two sessions compete for it.
        let commit_files: Vec<String> = (0..10).map(|i| format!("f{i}.rs")).collect();

        // Reader: touched 3 of the 10 files, all Reads. Old formula:
        // precision = 0.4*3 / 10 = 0.12, recall = 0.4*3 / 0.4*3 = 1.0,
        // overlap = max(0.12, 1.0) = 1.0 → confidence ≈ 0.75 (winner).
        let reader = transcript(
            Provider::Claude,
            "reader",
            "/repo",
            start,
            end,
            vec![
                ("/repo/f0.rs", TouchKind::Read),
                ("/repo/f1.rs", TouchKind::Read),
                ("/repo/f2.rs", TouchKind::Read),
            ],
        );
        // Author: wrote 3 of the 10 files. Precision = 1*3 / 10 = 0.30,
        // recall = 1*3 / 1*3 = 1.0, overlap = 1.0 → confidence ≈ 0.75.
        let author = transcript(
            Provider::Claude,
            "author",
            "/repo",
            start,
            end,
            vec![
                ("/repo/f3.rs", TouchKind::Write),
                ("/repo/f4.rs", TouchKind::Write),
                ("/repo/f5.rs", TouchKind::Write),
            ],
        );
        let commit = commit_entry("abc", when, "msg", ("Me", "me@x"));
        let ranked =
            TranscriptMatcher::new(&[reader, author], "/repo").score_commit(&commit, &commit_files);
        assert!(!ranked.is_empty(), "expected at least one candidate");
        assert_eq!(
            ranked.first().unwrap().session_id,
            "author",
            "authoring session must outrank a read-only bystander; \
             got ranking {ranked:?}"
        );
    }

    #[test]
    fn wrong_cwd_gates_out_even_with_perfect_overlap() {
        let start = Utc::now() - Duration::minutes(30);
        let end = Utc::now() - Duration::minutes(5);
        let when = Utc::now() - Duration::minutes(10);
        let t = transcript(
            Provider::Claude,
            "wrong-cwd",
            "/other/project",
            start,
            end,
            vec![("/repo/a.rs", TouchKind::Write)],
        );
        let commit = commit_entry("abc", when, "msg", ("Me", "me@x"));
        let m = TranscriptMatcher::new(&[t], "/repo").score_commit(&commit, &["a.rs".into()]);
        assert!(m.is_empty(), "cwd gate should reject, got {m:?}");
    }

    #[test]
    fn lineage_anchor_rescues_squash_merge_attribution() {
        // Setup: a codex worktree session that ended weeks ago, but
        // whose work landed on main today via squash merge. The merge
        // commit is a descendant of the session's starting_commit, so
        // the lineage anchor should rescue the match even though the
        // strict time gate rejects it.
        let session_start = Utc::now() - Duration::weeks(3);
        let session_end = Utc::now() - Duration::weeks(3) + Duration::minutes(45);
        let merge_when = Utc::now() - Duration::minutes(5); // landed today
        let starting_commit = "ffff".repeat(10);
        let merge_commit_sha = "abc".to_string();

        let mut t = transcript(
            Provider::Codex,
            "codex-on-worktree",
            "/repo",
            session_start,
            session_end,
            vec![
                ("/repo/src/auth.rs", TouchKind::Write),
                ("/repo/src/scope.rs", TouchKind::Write),
            ],
        );
        t.starting_commit = Some(starting_commit.clone());

        let merge = commit_entry(
            &merge_commit_sha,
            merge_when,
            "Squash merge: feature/auth-scope (#42)\n\nCo-Authored-By: codex <codex@openai.com>",
            ("Reviewer", "rev@x"),
        );

        // No lineage anchors → strict time gate filters us out, even
        // though file overlap is perfect.
        let strict = TranscriptMatcher::new(std::slice::from_ref(&t), "/repo")
            .score_commit(&merge, &["src/auth.rs".into(), "src/scope.rs".into()]);
        assert!(
            strict.is_empty(),
            "expected strict-gate rejection without lineage anchors, got {strict:?}"
        );

        // With a lineage anchor saying the merge commit descends from
        // the session's start, the same session now matches and scores
        // strongly via file_overlap + lineage_fit + provider_hint.
        let mut anchors: HashMap<usize, HashSet<String>> = HashMap::new();
        anchors.insert(0, HashSet::from([merge_commit_sha.clone()]));
        let lineage_aware = TranscriptMatcher::new(std::slice::from_ref(&t), "/repo")
            .with_lineage_anchors(anchors)
            .score_commit(&merge, &["src/auth.rs".into(), "src/scope.rs".into()]);
        assert_eq!(lineage_aware.len(), 1, "got {lineage_aware:?}");
        let m = &lineage_aware[0];
        assert_eq!(m.session_id, "codex-on-worktree");
        // File overlap is full → 0.55. Time fit 0 → 0. Lineage 1 →
        // 0.15. Provider hint 1 (codex Co-Authored-By) → 0.10.
        // Total ≈ 0.80.
        assert!(
            (m.lineage_fit - 1.0).abs() < 1e-6,
            "lineage_fit should be 1.0, got {}",
            m.lineage_fit
        );
        assert!(
            m.time_fit < 0.05,
            "time_fit should be ~0 (off-window), got {}",
            m.time_fit
        );
        assert!(
            m.confidence > 0.7,
            "lineage-only match should score >0.7, got {}",
            m.confidence
        );
    }

    #[test]
    fn outside_time_window_returns_empty() {
        let start = Utc::now() - Duration::hours(5);
        let end = Utc::now() - Duration::hours(4);
        let when = Utc::now(); // well outside grace
        let t = transcript(
            Provider::Claude,
            "too-old",
            "/repo",
            start,
            end,
            vec![("/repo/a.rs", TouchKind::Write)],
        );
        let commit = commit_entry("abc", when, "msg", ("Me", "me@x"));
        let m = TranscriptMatcher::new(&[t], "/repo").score_commit(&commit, &["a.rs".into()]);
        assert!(m.is_empty());
    }

    #[test]
    fn provider_hint_from_trailer_breaks_ties() {
        let start = Utc::now() - Duration::minutes(30);
        let end = Utc::now() - Duration::minutes(5);
        let when = Utc::now() - Duration::minutes(10);

        let claude_t = transcript(
            Provider::Claude,
            "claude",
            "/repo",
            start,
            end,
            vec![("/repo/a.rs", TouchKind::Write)],
        );
        let codex_t = transcript(
            Provider::Codex,
            "codex",
            "/repo",
            start,
            end,
            vec![("/repo/a.rs", TouchKind::Write)],
        );
        let commit = commit_entry(
            "abc",
            when,
            "msg\n\nCo-Authored-By: Claude <claude@anthropic.com>",
            ("Me", "me@x"),
        );
        // Both sessions have identical overlap + time — the provider
        // hint should steer the winner to Claude.
        let ranked = TranscriptMatcher::new(&[claude_t, codex_t], "/repo")
            .score_commit(&commit, &["a.rs".into()]);
        assert_eq!(ranked.first().unwrap().provider, Provider::Claude);
        assert!(ranked[0].confidence > ranked[1].confidence);
    }

    #[test]
    fn no_overlap_below_threshold_returns_nothing() {
        let start = Utc::now() - Duration::minutes(30);
        let end = Utc::now() - Duration::minutes(5);
        let when = Utc::now() - Duration::minutes(10);
        let t = transcript(
            Provider::Claude,
            "no-overlap",
            "/repo",
            start,
            end,
            vec![("/repo/unrelated.rs", TouchKind::Write)],
        );
        let commit = commit_entry("abc", when, "msg", ("Me", "me@x"));
        let m = TranscriptMatcher::new(&[t], "/repo").score_commit(&commit, &["a.rs".into()]);
        // time_fit = 1.0 * 0.25 = 0.25 + 0.5 * 0.10 hint = 0.30 < 0.35 threshold.
        assert!(m.is_empty(), "expected gated by min_confidence, got {m:?}");
    }

    #[test]
    fn session_in_subdir_still_counts_as_related() {
        let start = Utc::now() - Duration::minutes(30);
        let end = Utc::now() - Duration::minutes(5);
        let when = Utc::now() - Duration::minutes(10);
        let t = transcript(
            Provider::Claude,
            "subdir",
            "/repo/crates/foo",
            start,
            end,
            vec![("/repo/crates/foo/a.rs", TouchKind::Write)],
        );
        let commit = commit_entry("abc", when, "msg", ("Me", "me@x"));
        let m = TranscriptMatcher::new(&[t], "/repo")
            .score_commit(&commit, &["crates/foo/a.rs".into()]);
        assert!(!m.is_empty());
    }

    #[test]
    fn worktree_prefixed_relative_touch_matches_repo_relative_commit_path() {
        let start = Utc::now() - Duration::minutes(30);
        let end = Utc::now() - Duration::minutes(5);
        let when = Utc::now() - Duration::minutes(10);
        let t = transcript(
            Provider::Claude,
            "worktree-prefix",
            "/repo",
            start,
            end,
            vec![(
                ".codex/worktrees/branch-123/crates/foo/a.rs",
                TouchKind::Write,
            )],
        );
        let commit = commit_entry("abc", when, "msg", ("Me", "me@x"));
        let m = TranscriptMatcher::new(&[t], "/repo")
            .score_commit(&commit, &["crates/foo/a.rs".into()]);
        assert!(
            !m.is_empty(),
            "worktree-prefixed relative paths should match"
        );
    }

    #[test]
    fn parent_cwd_and_suffix_overlap_do_not_match_repo_files() {
        let start = Utc::now() - Duration::minutes(30);
        let end = Utc::now() - Duration::minutes(5);
        let when = Utc::now() - Duration::minutes(10);
        let t = transcript(
            Provider::Claude,
            "parent-cwd",
            "/repo",
            start,
            end,
            vec![("/repo/other/src/auth.rs", TouchKind::Write)],
        );
        let commit = commit_entry("abc", when, "msg", ("Me", "me@x"));
        let m = TranscriptMatcher::new(&[t], "/repo/project")
            .score_commit(&commit, &["src/auth.rs".into()]);
        assert!(m.is_empty(), "parent cwd should be rejected, got {m:?}");
    }

    #[test]
    fn suffix_only_overlap_is_not_treated_as_repo_local() {
        let start = Utc::now() - Duration::minutes(30);
        let end = Utc::now() - Duration::minutes(5);
        let when = Utc::now() - Duration::minutes(10);
        let t = transcript(
            Provider::Claude,
            "suffix-only",
            "/repo/project",
            start,
            end,
            vec![("/elsewhere/src/auth.rs", TouchKind::Write)],
        );
        let commit = commit_entry("abc", when, "msg", ("Me", "me@x"));
        let m = TranscriptMatcher::new(&[t], "/repo/project")
            .score_commit(&commit, &["src/auth.rs".into()]);
        assert!(m.is_empty(), "suffix overlap should not count, got {m:?}");
    }

    #[test]
    fn top_n_is_respected() {
        let start = Utc::now() - Duration::minutes(30);
        let end = Utc::now() - Duration::minutes(5);
        let when = Utc::now() - Duration::minutes(10);
        let ts = (0..5)
            .map(|i| {
                transcript(
                    Provider::Claude,
                    &format!("s{i}"),
                    "/repo",
                    start,
                    end,
                    vec![("/repo/a.rs", TouchKind::Write)],
                )
            })
            .collect::<Vec<_>>();
        let commit = commit_entry("abc", when, "msg", ("Me", "me@x"));
        let params = MatchParams {
            top_n: 2,
            ..MatchParams::default()
        };
        let m = TranscriptMatcher::new(&ts, "/repo")
            .with_params(params)
            .score_commit(&commit, &["a.rs".into()]);
        assert_eq!(m.len(), 2);
    }
}
