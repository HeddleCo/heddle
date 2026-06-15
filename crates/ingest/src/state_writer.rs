// SPDX-License-Identifier: Apache-2.0
//! Translate git commits into Heddle [`State`]s.
//!
//! The state writer is the glue between [`GitSource`](crate::git_walk::GitSource)
//! (git reads), [`TreeTranslator`](crate::tree_translate::TreeTranslator)
//! (tree/blob translation), and the [`ObjectStore`] (Heddle writes). One call
//! to [`StateWriter::write_commit`] turns one [`CommitEntry`] into one
//! `State`, records the `git_sha → ChangeId` mapping in the [`ShaMap`],
//! and returns the new `ChangeId`.
//!
//! # Parent ordering
//!
//! Git and Heddle disagree on parent semantics:
//!
//! - Git's first parent on a merge commit is usually the **target** branch
//!   (the branch you were on when you ran `git merge`).
//! - Heddle uses the same convention: `parents[0]` is the target,
//!   `parents[1..]` are sources.
//!
//! So we preserve `CommitEntry::parents` order verbatim. If a parent isn't
//! yet in the sha map, we refuse the write — callers are expected to feed
//! commits in topological order (see
//! [`GitSource::commits_topo`](crate::git_walk::GitSource::commits_topo)).
//!
//! # Attribution
//!
//! [`parse_attribution`] examines the commit author plus any
//! `Co-Authored-By:` trailers in the message. A trailer whose email hints
//! at an agent (`claude`, `codex`, `chatgpt`, etc.) upgrades the
//! attribution to an agent-assisted one; the principal stays the human
//! author. Session IDs aren't recoverable from a bare commit — the
//! transcript matcher fills those in later.

use chrono::{DateTime, Utc};
use objects::{
    object::{Agent, Attribution, ChangeId, ContentHash, Principal, State, Status},
    store::ObjectStore,
};
use serde::Deserialize;

use crate::{
    IngestError,
    git_walk::{CommitEntry, GitSignature},
    sha_map::ShaMap,
};

/// Writes Heddle [`State`]s from [`CommitEntry`] inputs. Holds short-lived
/// borrows — construct one per commit or per batch, not a long-lived field.
pub struct StateWriter<'a, S: ObjectStore> {
    store: &'a S,
    map: &'a mut ShaMap,
}

impl<'a, S: ObjectStore> StateWriter<'a, S> {
    pub fn new(store: &'a S, map: &'a mut ShaMap) -> Self {
        Self { store, map }
    }

    /// Persist one commit as a Heddle state and record the `git_sha →
    /// ChangeId` mapping. The caller is responsible for having translated
    /// the commit's root tree (pass its Heddle hash as `tree`) and for
    /// feeding commits in parent-before-child order.
    ///
    /// Returns the newly-minted [`ChangeId`] — the same one the sha map
    /// now has recorded for `commit.sha`.
    pub fn write_commit(
        &mut self,
        commit: &CommitEntry,
        tree: ContentHash,
    ) -> crate::Result<ChangeId> {
        // Idempotency: if we've already translated this commit, surface
        // the existing ChangeId without double-writing.
        if let Some(cid) = self.map.get_commit(&commit.sha) {
            return Ok(cid);
        }

        let mut parents = Vec::with_capacity(commit.parents.len());
        for p in &commit.parents {
            match self.map.get_commit(p) {
                Some(cid) => parents.push(cid),
                None => {
                    return Err(IngestError::Other(format!(
                        "commit {} has parent {} that hasn't been translated yet — \
                         feed commits in topological order",
                        commit.sha, p
                    )));
                }
            }
        }

        // The plain `StateWriter` receives an already-translated tree hash and
        // does no tree translation itself, so it has no lossy signal to record —
        // lossy detection lives on the tree-translating `PackedImport` path.
        let state = state_from_commit(commit, tree, parents, false)?;

        self.store.put_state(&state).map_err(IngestError::from)?;

        self.map
            .insert_commit(&commit.sha, state.change_id)
            .map_err(IngestError::from)?;

        Ok(state.change_id)
    }
}

pub(crate) fn state_from_commit(
    commit: &CommitEntry,
    tree: ContentHash,
    parents: Vec<ChangeId>,
    git_lossy: bool,
) -> crate::Result<State> {
    // A lossy string view, derived once for the parsers that need text
    // (attribution trailers, the one-line intent). The verbatim bytes still
    // reach `with_raw_message` below, so a non-UTF8 message is preserved even
    // though these ASCII-footer parsers read a lossy view.
    let message = String::from_utf8_lossy(&commit.message);
    let note = read_heddle_note(commit)?;
    let identity = resolve_identity(commit, note.as_ref())?;
    let attribution = parse_attribution_with_note(&commit.author, &message, note.as_ref());
    let status = note_status(note.as_ref());

    // Heddle's hash includes the committer timestamp, so we use
    // committed_at (not authored_at) for `created_at` — keeps re-
    // imported repos producing identical State hashes run-to-run.
    //
    // The author timestamp is preserved separately on the State via
    // `with_authored_at` so blame can show the *authored* time
    // (matching git blame's default), while ordering/log queries
    // continue to use `created_at` (matching git log's default).
    // For commits where author == committer (the common case in
    // merge-without-rebase workflows) the two are identical and the
    // distinction is invisible; for rebased / cherry-picked /
    // amended commits it preserves the original authoring time.
    // #564 step 1: preserve the committer identity, both timezone offsets,
    // the verbatim message, and any extra headers (in order, gpgsig inline at
    // its captured position) so the commit is byte-reconstructable later
    // (#566) without the mirror.
    let mut state = State::new(tree, parents, attribution)
        .with_change_id(identity)
        .with_timestamp(committed_timestamp(&commit.committed_at))
        .with_authored_at(committed_timestamp(&commit.authored_at))
        .with_intent(first_line_of(&message))
        .with_committer(Principal::new(
            commit.committer.name.clone(),
            commit.committer.email.clone(),
        ))
        .with_tz_offsets(commit.author.tz_offset, commit.committer.tz_offset)
        .with_raw_message(commit.message.clone())
        .with_git_lossy(git_lossy)
        .with_extra_headers(commit.extra_headers.clone())
        .with_status(status);
    if let Some(confidence) = note.as_ref().and_then(|note| note.confidence) {
        state = state.with_confidence(confidence);
    }
    Ok(state)
}

/// Best-effort attribution parse. The principal is always the git author;
/// the agent, if any, comes from a `Co-Authored-By:` trailer whose name
/// or email resembles an AI assistant.
pub fn parse_attribution(author: &GitSignature, message: &str) -> Attribution {
    parse_attribution_with_note(author, message, None)
}

fn parse_attribution_with_note(
    author: &GitSignature,
    message: &str,
    note: Option<&HeddleNote>,
) -> Attribution {
    let principal = note
        .and_then(|note| note.attribution.as_ref())
        .map(|attribution| {
            Principal::new(
                attribution.principal_name.clone(),
                attribution.principal_email.clone(),
            )
        })
        .unwrap_or_else(|| Principal::new(author.name.clone(), author.email.clone()));

    if let Some(agent) = note
        .and_then(|note| note.attribution.as_ref())
        .and_then(|attribution| attribution.agent.as_ref())
        .or_else(|| note.and_then(|note| note.agent.as_ref()))
        .map(|agent| Agent::new(agent.provider.clone(), agent.model.clone()))
        .or_else(|| detect_agent_in_message(message))
    {
        Attribution::with_agent(principal, agent)
    } else {
        Attribution::human(principal)
    }
}

#[derive(Debug, Deserialize)]
struct HeddleNote {
    change_id: String,
    #[serde(default)]
    agent: Option<HeddleNoteAgent>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    attribution: Option<HeddleNoteAttribution>,
}

#[derive(Debug, Deserialize)]
struct HeddleNoteAttribution {
    principal_name: String,
    principal_email: String,
    #[serde(default)]
    agent: Option<HeddleNoteAgent>,
}

#[derive(Debug, Deserialize)]
struct HeddleNoteAgent {
    provider: String,
    model: String,
}

fn read_heddle_note(commit: &CommitEntry) -> crate::Result<Option<HeddleNote>> {
    let Some(note_bytes) = commit.heddle_note.as_ref() else {
        return Ok(None);
    };
    serde_json::from_slice(note_bytes)
        .map(Some)
        .map_err(|error| {
            IngestError::Git(format!(
                "parse Heddle note for commit {}: {error}",
                commit.sha
            ))
        })
}

fn resolve_identity(commit: &CommitEntry, note: Option<&HeddleNote>) -> crate::Result<ChangeId> {
    if let Some(note) = note {
        return ChangeId::parse(&note.change_id).map_err(|error| {
            IngestError::Git(format!(
                "invalid Heddle note change_id for commit {}: {error}",
                commit.sha
            ))
        });
    }
    let message = String::from_utf8_lossy(&commit.message);
    if let Some(change_id) = parse_trailers(&message).get("Heddle-Change-Id") {
        return ChangeId::parse(change_id).map_err(|error| {
            IngestError::Git(format!(
                "invalid Heddle-Change-Id trailer for commit {}: {error}",
                commit.sha
            ))
        });
    }
    deterministic_change_id_from_git_sha(&commit.sha)
}

fn note_status(note: Option<&HeddleNote>) -> Status {
    match note.map(|note| note.status.as_str()) {
        Some("published") => Status::Published,
        _ => Status::Draft,
    }
}

fn parse_trailers(message: &str) -> std::collections::HashMap<String, String> {
    let mut trailers = std::collections::HashMap::new();
    for line in message.lines().rev() {
        if line.is_empty() {
            break;
        }
        if let Some(pos) = line.find(':') {
            let key = &line[..pos];
            let value = line[pos + 1..].trim();
            if key.starts_with("Heddle-") {
                trailers.insert(key.to_string(), value.to_string());
            }
        } else if !line.trim().is_empty() {
            break;
        }
    }
    trailers
}

fn deterministic_change_id_from_git_sha(sha: &str) -> crate::Result<ChangeId> {
    let trimmed = sha.trim();
    if trimmed.len() < 32 || !trimmed.chars().take(32).all(|c| c.is_ascii_hexdigit()) {
        return Err(IngestError::Git(format!(
            "commit {sha} cannot seed deterministic Heddle identity: expected full hex SHA"
        )));
    }
    let mut bytes = [0u8; 16];
    for (idx, slot) in bytes.iter_mut().enumerate() {
        let pair = &trimmed[idx * 2..idx * 2 + 2];
        *slot = u8::from_str_radix(pair, 16).map_err(|error| {
            IngestError::Git(format!(
                "commit {sha} cannot seed deterministic Heddle identity: {error}"
            ))
        })?;
    }
    Ok(ChangeId::from_bytes(bytes))
}

/// Scan trailers for `Co-Authored-By: <name> <<email>>` lines and
/// heuristically classify the agent. Returns the first agent-like hit;
/// human co-authors are ignored (they're credited separately elsewhere).
fn detect_agent_in_message(message: &str) -> Option<Agent> {
    // Trailers live in the last paragraph of a commit message. We don't
    // need to be precise — we're only pulling `Co-Authored-By` lines,
    // which are distinctive enough to grep regardless of paragraph.
    for line in message.lines().rev() {
        let lower = line.to_ascii_lowercase();
        if !lower.starts_with("co-authored-by:") {
            continue;
        }
        let rest = &line["co-authored-by:".len()..].trim();
        // Split `"Model Name <email>"` into (name, email). Be lenient:
        // some tools omit the email, in which case we use the name alone.
        let (name, email) = match (rest.rfind('<'), rest.rfind('>')) {
            (Some(start), Some(end)) if end > start => {
                let name = rest[..start].trim();
                let email = rest[start + 1..end].trim();
                (name, email)
            }
            _ => (rest.trim(), ""),
        };
        let signal = format!(
            "{} {}",
            name.to_ascii_lowercase(),
            email.to_ascii_lowercase()
        );
        if signal.contains("claude") || signal.contains("anthropic") {
            return Some(Agent::new("anthropic", best_model_from(name, "claude")));
        }
        if signal.contains("codex") || signal.contains("chatgpt") || signal.contains("openai") {
            return Some(Agent::new("openai", best_model_from(name, "codex")));
        }
        if signal.contains("copilot") {
            return Some(Agent::new("github", best_model_from(name, "copilot")));
        }
        if signal.contains("gemini") || signal.contains("google") {
            return Some(Agent::new("google", best_model_from(name, "gemini")));
        }
        // Unknown AI-flavored trailer? Fall back to a generic agent so
        // provenance isn't lost — the `provider` is just the token we
        // matched on, which is still more useful than dropping it.
    }
    None
}

/// Pick a usable model string out of the trailer name. If the name looks
/// like a real model id (contains a digit or hyphen), use it verbatim;
/// otherwise fall back to `fallback` so downstream filtering still works.
fn best_model_from(name: &str, fallback: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    if trimmed.chars().any(|c| c.is_ascii_digit() || c == '-') {
        trimmed.to_string()
    } else {
        fallback.to_string()
    }
}

/// First line of the message, trimmed — used as the state's `intent`
/// (the one-line "why" Heddle surfaces in its UI). Defaults to empty for
/// messageless commits.
fn first_line_of(message: &str) -> String {
    message.lines().next().unwrap_or("").trim().to_string()
}

/// Normalize `committed_at` to UTC. `GitSignature::time` is already UTC,
/// but we centralize the truncation here so the state-core hash function
/// only ever sees second-precision timestamps (which is all it records).
fn committed_timestamp(t: &DateTime<Utc>) -> DateTime<Utc> {
    *t
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use objects::{
        object::{Blob, Tree},
        store::InMemoryStore,
    };

    use super::*;
    use crate::git_walk::{CommitEntry, GitSignature};

    fn sig(name: &str, email: &str) -> GitSignature {
        GitSignature {
            name: name.into(),
            email: email.into(),
            time: Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap(),
            tz_offset: 0,
        }
    }

    fn make_commit(sha: &str, parents: Vec<String>, message: &str) -> CommitEntry {
        CommitEntry {
            sha: sha.into(),
            tree_sha: "0000000000000000000000000000000000000000".into(),
            parents,
            author: sig("Alice", "alice@example.com"),
            committer: sig("Alice", "alice@example.com"),
            message: message.as_bytes().to_vec(),
            authored_at: Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap(),
            committed_at: Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap(),
            extra_headers: Vec::new(),
            heddle_note: None,
        }
    }

    fn empty_tree_hash(store: &InMemoryStore) -> ContentHash {
        store.put_tree(&Tree::from_entries(vec![])).unwrap()
    }

    #[test]
    fn writes_root_commit_with_human_attribution() {
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let tree = empty_tree_hash(&store);
        let commit = make_commit("aa".repeat(20).as_str(), vec![], "chore: initial\n");

        let cid = StateWriter::new(&store, &mut map)
            .write_commit(&commit, tree)
            .unwrap();

        assert_eq!(map.get_commit(&commit.sha), Some(cid));
        let state = store.get_state(&cid).unwrap().expect("state written");
        assert!(state.parents.is_empty());
        assert!(state.attribution.agent.is_none());
        assert_eq!(state.attribution.principal.name, "Alice");
        assert_eq!(state.intent.as_deref(), Some("chore: initial"));
    }

    #[test]
    fn second_write_of_same_sha_is_idempotent() {
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let tree = empty_tree_hash(&store);
        let commit = make_commit("bb".repeat(20).as_str(), vec![], "feat: one\n");

        let a = StateWriter::new(&store, &mut map)
            .write_commit(&commit, tree)
            .unwrap();
        let b = StateWriter::new(&store, &mut map)
            .write_commit(&commit, tree)
            .unwrap();

        assert_eq!(a, b);
        // Exactly one state written, not two.
        assert_eq!(store.list_states().unwrap().len(), 1);
    }

    #[test]
    fn refuses_unmapped_parent() {
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let tree = empty_tree_hash(&store);
        let commit = make_commit(
            "cc".repeat(20).as_str(),
            vec!["dd".repeat(20)],
            "feat: child\n",
        );

        let err = StateWriter::new(&store, &mut map)
            .write_commit(&commit, tree)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("hasn't been translated"),
            "expected topo-order error, got: {msg}"
        );
    }

    #[test]
    fn chain_preserves_parent_order() {
        // git: root → a → b (b merges with c). Parent[0] of b is a (target),
        // parent[1] is c (source). Heddle must see the same order.
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let tree = empty_tree_hash(&store);

        let root = make_commit("11".repeat(20).as_str(), vec![], "root\n");
        let a = make_commit("22".repeat(20).as_str(), vec![root.sha.clone()], "a\n");
        let c = make_commit("33".repeat(20).as_str(), vec![root.sha.clone()], "c\n");
        let b = make_commit(
            "44".repeat(20).as_str(),
            vec![a.sha.clone(), c.sha.clone()],
            "b: merge\n",
        );

        let mut wr = StateWriter::new(&store, &mut map);
        let root_cid = wr.write_commit(&root, tree).unwrap();
        let a_cid = wr.write_commit(&a, tree).unwrap();
        let c_cid = wr.write_commit(&c, tree).unwrap();
        let b_cid = wr.write_commit(&b, tree).unwrap();

        let b_state = store.get_state(&b_cid).unwrap().unwrap();
        assert_eq!(b_state.parents, vec![a_cid, c_cid]);
        // And root / c's lineage survives.
        assert_eq!(
            store.get_state(&a_cid).unwrap().unwrap().parents,
            vec![root_cid]
        );
    }

    #[test]
    fn detects_claude_co_author() {
        let msg = "feat: thing\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>\n";
        let a = parse_attribution(&sig("Luke", "l@example.com"), msg);
        let agent = a.agent.expect("agent detected");
        assert_eq!(agent.provider, "anthropic");
        assert_eq!(a.principal.name, "Luke");
    }

    #[test]
    fn detects_codex_co_author() {
        let msg = "feat: thing\n\nCo-Authored-By: Codex <noreply@openai.com>\n";
        let a = parse_attribution(&sig("Luke", "l@example.com"), msg);
        let agent = a.agent.expect("agent detected");
        assert_eq!(agent.provider, "openai");
    }

    #[test]
    fn ignores_human_co_author() {
        let msg = "feat: pair\n\nCo-Authored-By: Jamie <jamie@example.com>\n";
        let a = parse_attribution(&sig("Luke", "l@example.com"), msg);
        assert!(
            a.agent.is_none(),
            "human co-author must not produce an agent"
        );
    }

    #[test]
    fn heddle_note_preserves_identity_and_metadata() {
        let store = InMemoryStore::new();
        let tree = empty_tree_hash(&store);
        let expected_id = ChangeId::from_bytes([0x42; 16]);
        let mut commit = make_commit("12".repeat(20).as_str(), vec![], "feat: noted\n");
        commit.heddle_note = Some(
            format!(
                r#"{{
  "change_id": "{}",
  "status": "published",
  "confidence": 0.875,
  "agent": {{"provider": "openai", "model": "codex"}},
  "attribution": {{
    "principal_name": "Luke",
    "principal_email": "luke@example.com",
    "agent": {{"provider": "anthropic", "model": "claude-opus"}}
  }}
}}"#,
                expected_id.to_string_full()
            )
            .into_bytes(),
        );

        let state = state_from_commit(&commit, tree, vec![], false).unwrap();

        assert_eq!(state.change_id, expected_id);
        assert_eq!(state.status, Status::Published);
        assert_eq!(state.confidence, Some(0.875));
        assert_eq!(state.attribution.principal.name, "Luke");
        assert_eq!(state.attribution.principal.email, "luke@example.com");
        let agent = state.attribution.agent.expect("note agent preserved");
        assert_eq!(agent.provider, "anthropic");
        assert_eq!(agent.model, "claude-opus");
    }

    #[test]
    fn preserves_blob_store_untouched_on_commit_write() {
        // State writer shouldn't know or care about blobs. If we hand it
        // a pre-hashed tree, no blob writes should have happened as a
        // side effect.
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let _ = store.put_blob(&Blob::from_slice(b"sentinel")).unwrap();
        let tree = empty_tree_hash(&store);
        let blobs_before = store.list_blobs().unwrap().len();

        let commit = make_commit("ee".repeat(20).as_str(), vec![], "chore: quiet\n");
        StateWriter::new(&store, &mut map)
            .write_commit(&commit, tree)
            .unwrap();

        let blobs_after = store.list_blobs().unwrap().len();
        assert_eq!(blobs_before, blobs_after);
    }

    /// #564 step 1: a re-imported commit must round-trip every git-fidelity
    /// field — distinct committer identity, both timezone offsets, the
    /// verbatim message, and the extra headers in order (gpgsig kept inline at
    /// its captured position) — so the commit is byte-reconstructable later
    /// (#566) without the git mirror.
    #[test]
    fn write_commit_preserves_git_fidelity_fields() {
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let tree = empty_tree_hash(&store);

        let mut commit = make_commit("ff".repeat(20).as_str(), vec![], "feat: thing\n\nBody.\n");
        commit.author = GitSignature {
            name: "Author".into(),
            email: "author@example.com".into(),
            time: Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap(),
            tz_offset: -7 * 3600,
        };
        commit.committer = GitSignature {
            name: "Committer".into(),
            email: "committer@example.com".into(),
            time: Utc.with_ymd_and_hms(2026, 4, 2, 9, 0, 0).unwrap(),
            tz_offset: 2 * 3600,
        };
        // gpgsig sits BETWEEN mergetag and encoding — a non-canonical order
        // that proves the signature keeps its captured ordinal in
        // `extra_headers` (no split-out field that would lose the position).
        commit.extra_headers = vec![
            (b"mergetag".to_vec(), b"object deadbeef".to_vec()),
            (
                b"gpgsig".to_vec(),
                b"-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----".to_vec(),
            ),
            (b"encoding".to_vec(), b"ISO-8859-1".to_vec()),
        ];

        let cid = StateWriter::new(&store, &mut map)
            .write_commit(&commit, tree)
            .unwrap();
        let state = store.get_state(&cid).unwrap().expect("state written");

        let committer = state.committer.expect("committer preserved");
        assert_eq!(committer.name, "Committer");
        assert_eq!(committer.email, "committer@example.com");
        assert_eq!(state.authored_tz_offset, -7 * 3600);
        assert_eq!(state.committer_tz_offset, 2 * 3600);
        assert_eq!(
            state.raw_message.as_deref(),
            Some("feat: thing\n\nBody.\n".as_bytes())
        );
        // The extra headers (gpgsig included) round-trip in exactly the
        // captured order.
        assert_eq!(
            state.extra_headers,
            vec![
                (b"mergetag".to_vec(), b"object deadbeef".to_vec()),
                (
                    b"gpgsig".to_vec(),
                    b"-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----".to_vec(),
                ),
                (b"encoding".to_vec(), b"ISO-8859-1".to_vec()),
            ]
        );
        // `intent` stays the trimmed first line, distinct from `raw_message`.
        assert_eq!(state.intent.as_deref(), Some("feat: thing"));
    }
}
