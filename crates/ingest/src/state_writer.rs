// SPDX-License-Identifier: Apache-2.0
//! Local adapter for the canonical Git import State converter.

use objects::object::{
    Attribution, ContentHash, State, StateId,
    thread_replication::{
        git_import_converter::{
            GitImportCommit, GitImportGraph, GitImportParentPolicy, GitImportSignature,
            parse_git_attribution,
        },
        git_import_graph::GitObjectId,
    },
};

use crate::{
    IngestError,
    git_walk::{CommitEntry, GitSignature},
};

#[cfg(test)]
fn state_from_commit(
    commit: &CommitEntry,
    tree: ContentHash,
    parents: Vec<StateId>,
    git_lossy: bool,
) -> crate::Result<State> {
    state_from_commit_with_rewrites(commit, tree, parents, git_lossy, |_| Ok(None))
}

pub(crate) fn state_from_commit_with_rewrites(
    commit: &CommitEntry,
    tree: ContentHash,
    parents: Vec<StateId>,
    git_lossy: bool,
    rewritten_parent: impl Fn(StateId) -> crate::Result<Option<StateId>>,
) -> crate::Result<State> {
    convert(
        commit,
        tree,
        parents,
        git_lossy,
        GitImportParentPolicy::Validate,
        rewritten_parent,
    )
}

/// A lazy overlay has not materialized the Git parent graph. Preserve its
/// embedded source State until the full import validates actual parents.
pub(crate) fn descriptor_state_from_commit(
    commit: &CommitEntry,
    tree: ContentHash,
    git_lossy: bool,
) -> crate::Result<State> {
    convert(
        commit,
        tree,
        Vec::new(),
        git_lossy,
        GitImportParentPolicy::PreserveEmbedded,
        |_| Ok(None),
    )
}

fn convert(
    commit: &CommitEntry,
    tree: ContentHash,
    parents: Vec<StateId>,
    git_lossy: bool,
    parent_policy: GitImportParentPolicy,
    rewritten_parent: impl Fn(StateId) -> crate::Result<Option<StateId>>,
) -> crate::Result<State> {
    let oid = parse_git_oid(&commit.sha)?;
    GitImportGraph::convert_commit(
        GitImportCommit {
            oid: &oid,
            author: signature(&commit.author, commit.authored_at),
            committer: signature(&commit.committer, commit.committed_at),
            message: &commit.message,
            extra_headers: &commit.extra_headers,
            heddle_note: commit.heddle_note.as_deref(),
        },
        tree,
        parents,
        git_lossy,
        parent_policy,
        |state| {
            rewritten_parent(state)
                .map_err(|error| objects::error::HeddleError::InvalidObject(error.to_string()))
        },
    )
    .map_err(IngestError::from)
}

fn signature(value: &GitSignature, time: chrono::DateTime<chrono::Utc>) -> GitImportSignature {
    GitImportSignature {
        name: value.name.clone(),
        email: value.email.clone(),
        time,
        tz_offset: value.tz_offset,
    }
}

fn parse_git_oid(sha: &str) -> crate::Result<GitObjectId> {
    if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(IngestError::Git(format!(
            "commit {sha} cannot seed deterministic Heddle identity: expected full hex SHA"
        )));
    }
    let mut bytes = Vec::with_capacity(sha.len() / 2);
    for pair in sha.as_bytes().as_chunks::<2>().0 {
        let digits = std::str::from_utf8(pair)
            .map_err(|error| IngestError::Git(format!("invalid Git OID {sha}: {error}")))?;
        bytes.push(
            u8::from_str_radix(digits, 16)
                .map_err(|error| IngestError::Git(format!("invalid Git OID {sha}: {error}")))?,
        );
    }
    match bytes.len() {
        20 => Ok(GitObjectId::Sha1(bytes.try_into().map_err(|_| {
            IngestError::Git(format!("invalid SHA-1 Git OID {sha}"))
        })?)),
        32 => Ok(GitObjectId::Sha256(bytes.try_into().map_err(|_| {
            IngestError::Git(format!("invalid SHA-256 Git OID {sha}"))
        })?)),
        _ => Err(IngestError::Git(format!("invalid Git OID {sha}"))),
    }
}

/// Parse author attribution through the same converter used by hosted import.
pub fn parse_attribution(author: &GitSignature, message: &str) -> Attribution {
    parse_git_attribution(&signature(author, author.time), message, None)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        io::Write,
        path::Path,
        process::{Command, Stdio},
    };

    use chrono::{TimeZone, Utc};
    use objects::object::{
        ChangeId, ContentHash, HeddleNote, Principal, Status,
        thread_replication::{
            git_import_converter::{GitImportGraph, GitImportRawCommit},
            git_import_graph::{
                GitObjectFormat, ImportRefDisposition, ImportSkipReason,
                classify_frozen_import_refs,
            },
        },
    };
    use tempfile::TempDir;

    use super::*;
    use crate::git_walk::{CommitEntry, GitSignature, GitSource};

    fn git(path: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn local_and_raw_conversion_match_for_branched_merged_and_tag_only_graph() {
        let temp = TempDir::new().expect("fixture directory");
        let path = temp.path();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.name", "Test"]);
        git(path, &["config", "user.email", "test@example.com"]);
        std::fs::write(path.join("root"), b"root\n").expect("root file");
        git(path, &["add", "root"]);
        git(path, &["commit", "-qm", "root"]);
        git(path, &["switch", "-qc", "feature"]);
        std::fs::write(path.join("feature"), b"feature\n").expect("feature file");
        git(path, &["add", "feature"]);
        git(path, &["commit", "-qm", "feature"]);
        git(path, &["switch", "-q", "main"]);
        std::fs::write(path.join("main"), b"main\n").expect("main file");
        git(path, &["add", "main"]);
        git(path, &["commit", "-qm", "main"]);
        git(path, &["merge", "-q", "--no-ff", "feature", "-m", "merge"]);
        git(path, &["branch", "shared"]);
        git(path, &["tag", "light"]);
        git(path, &["tag", "-am", "annotated", "annotated"]);
        git(path, &["tag", "-am", "tag of tag", "outer", "annotated"]);
        git(path, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let blob = String::from_utf8(git(path, &["hash-object", "-w", "root"])).expect("blob OID");
        git(path, &["tag", "-am", "blob tag", "blob-tag", blob.trim()]);
        let tree = String::from_utf8(git(path, &["rev-parse", "HEAD^{tree}"])).expect("tree OID");
        let parent = String::from_utf8(git(path, &["rev-parse", "HEAD"])).expect("parent OID");
        let tagged = String::from_utf8(git(
            path,
            &[
                "commit-tree",
                tree.trim(),
                "-p",
                parent.trim(),
                "-m",
                "tag only",
            ],
        ))
        .expect("tag-only OID");
        git(path, &["tag", "tag-only", tagged.trim()]);

        let source = GitSource::open(path).expect("source");
        let frozen = source.collect_frozen_import_refs().expect("frozen refs");
        let classified = classify_frozen_import_refs(&frozen).expect("classify refs");
        assert_eq!(
            classified
                .dispositions
                .iter()
                .filter(|value| **value == ImportRefDisposition::Branch)
                .count(),
            3
        );
        assert_eq!(
            classified
                .dispositions
                .iter()
                .filter(|value| **value == ImportRefDisposition::CommitTag)
                .count(),
            4
        );
        assert_eq!(
            classified
                .skipped_refs
                .iter()
                .find(|reference| reference.raw_name == b"refs/tags/blob-tag")
                .map(|reference| reference.reason),
            Some(ImportSkipReason::NonCommitTag)
        );
        assert_eq!(
            classified
                .skipped_refs
                .iter()
                .find(|reference| reference.raw_name == b"refs/remotes/origin/main")
                .map(|reference| reference.reason),
            Some(ImportSkipReason::RemoteTracking)
        );
        let heads = source.collect_refs().expect("native refs");
        let commits = source
            .commits_topo(heads.iter().map(|head| head.target_sha.clone()))
            .expect("reachable graph");
        assert_eq!(commits.len(), 5);
        assert!(commits.iter().any(|commit| commit.parents.len() == 2));
        assert!(commits.iter().any(|commit| commit.sha == tagged.trim()));
        let native = TempDir::new().expect("native destination");
        let (stats, local_map) =
            crate::import_git_into(path, native.path()).expect("local import path");
        assert_eq!(stats.commits_imported, 5);
        assert_eq!(local_map.commit_count().expect("complete local map"), 5);
        let imported_repo = repo::Repository::open(native.path()).expect("native repository");
        let mut hosted_map = HashMap::new();
        let mut local_ids = HashMap::new();
        let mut hosted_states = HashMap::new();
        for commit in commits {
            let parents = commit
                .parents
                .iter()
                .map(|sha| *hosted_map.get(sha).expect("hosted parent converted"))
                .collect::<Vec<_>>();
            let local_id = local_map
                .get_commit(&commit.sha)
                .expect("local map")
                .expect("converted commit");
            let local = imported_repo
                .store()
                .get_state(&local_id)
                .expect("stored State")
                .expect("published State");
            let raw = git(path, &["cat-file", "commit", &commit.sha]);
            let oid = parse_git_oid(&commit.sha).expect("Git OID");
            let hosted = GitImportGraph::convert_raw_commit(
                GitImportRawCommit {
                    oid: &oid,
                    object_format: GitObjectFormat::Sha1,
                    raw_commit: &raw,
                    heddle_note: None,
                },
                local.tree,
                parents.clone(),
                false,
                |_| Ok(None),
            )
            .expect("hosted executor entry point");
            assert_eq!(
                local.parents, parents,
                "ordered Git parents for {}",
                commit.sha
            );
            assert_eq!(local, hosted, "commit {}", commit.sha);
            local_ids.insert(commit.sha.clone(), local_id);
            hosted_map.insert(commit.sha.clone(), hosted.id());
            hosted_states.insert(commit.sha, hosted);
        }
        assert_eq!(local_ids, hosted_map, "complete Git OID → StateId maps");
        use objects::{object::ThreadName, store::ObjectStore as _};
        for (name, git_ref) in [
            ("main", "refs/heads/main"),
            ("feature", "refs/heads/feature"),
            ("shared", "refs/heads/shared"),
        ] {
            let sha = String::from_utf8(git(path, &["rev-parse", git_ref])).expect("tip OID");
            let local_tip = imported_repo
                .refs()
                .get_thread(&ThreadName::new(name))
                .expect("published tip")
                .expect("Thread");
            let hosted_tip = &hosted_states[sha.trim()];
            assert_eq!(local_tip, hosted_tip.id());
            let local = imported_repo
                .store()
                .get_state(&local_tip)
                .expect("tip State")
                .expect("tip");
            assert_eq!(local, *hosted_tip);
            if name == "main" {
                assert_eq!(local.parents.len(), 2, "merge parent vector is preserved");
            }
        }
        assert!(
            hosted_states.contains_key(tagged.trim()),
            "tag-only commit belongs to both maps"
        );
    }

    #[test]
    #[ignore = "10k Git fixture and RSS measurement; run explicitly for import scaling"]
    fn ten_thousand_commit_graph_rss() {
        let temp = TempDir::new().expect("fixture directory");
        let path = temp.path();
        git(path, &["init", "-q", "-b", "main"]);
        let mut input = Vec::new();
        input.extend_from_slice(b"blob\nmark :1\ndata 2\nx\n");
        for index in 0..10_000 {
            write!(
                input,
                "commit refs/heads/main\nmark :{}\nauthor Test <test@example.com> {} +0000\ncommitter Test <test@example.com> {} +0000\ndata {}\ncommit {index}\n",
                index + 2,
                index + 1,
                index + 1,
                format!("commit {index}").len(),
            )
            .expect("fast-import input");
            if index == 0 {
                input.extend_from_slice(b"M 100644 :1 file\n");
            }
        }
        let mut child = Command::new("git")
            .arg("fast-import")
            .current_dir(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git fast-import");
        child
            .stdin
            .take()
            .expect("fast-import stdin")
            .write_all(&input)
            .expect("write graph");
        let output = child.wait_with_output().expect("fast-import result");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let source = GitSource::open(path).expect("source");
        let heads = source.collect_refs().expect("heads");
        let commits = source
            .commits_topo(heads.iter().map(|head| head.target_sha.clone()))
            .expect("10k graph");
        assert_eq!(commits.len(), 10_000);
        let mut mapped = HashMap::new();
        for commit in commits {
            let parents = commit
                .parents
                .iter()
                .map(|sha| *mapped.get(sha).expect("mapped parent"))
                .collect();
            let state = state_from_commit(&commit, empty_tree_hash(), parents, false)
                .expect("convert commit");
            mapped.insert(commit.sha, state.id());
        }
        assert_eq!(mapped.len(), 10_000);
        #[cfg(target_os = "linux")]
        {
            let status = std::fs::read_to_string("/proc/self/status").expect("process status");
            let rss_line = status
                .lines()
                .find(|line| line.starts_with("VmHWM:"))
                .expect("peak RSS");
            let rss_kib: usize = rss_line
                .split_whitespace()
                .nth(1)
                .expect("RSS value")
                .parse()
                .expect("RSS integer");
            eprintln!("10k graph peak RSS: {rss_kib} KiB");
            assert!(rss_kib < 512 * 1024, "10k graph used {rss_kib} KiB");
        }
    }

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

    fn empty_tree_hash() -> ContentHash {
        ContentHash::compute(b"empty tree")
    }

    #[test]
    fn builds_root_commit_with_human_attribution() {
        let tree = empty_tree_hash();
        let commit = make_commit("aa".repeat(20).as_str(), vec![], "chore: initial\n");

        let state = state_from_commit(&commit, tree, vec![], false).unwrap();

        assert!(state.parents.is_empty());
        assert!(state.attribution.agent.is_none());
        assert_eq!(state.attribution.principal.name, b"Alice");
        assert_eq!(state.intent.as_deref(), Some("chore: initial"));
    }

    #[test]
    fn detects_claude_co_author() {
        let msg = "feat: thing\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>\n";
        let a = parse_attribution(&sig("Luke", "l@example.com"), msg);
        let agent = a.agent.expect("agent detected");
        assert_eq!(agent.provider, "anthropic");
        assert_eq!(a.principal.name, b"Luke");
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
        let tree = empty_tree_hash();
        let expected_id = ChangeId::from_bytes([0x42; 16]);
        let mut commit = make_commit("12".repeat(20).as_str(), vec![], "feat: noted\n");
        commit.heddle_note = Some(
            format!(
                r#"{{
  "state_id": "{}",
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
                StateId::from_bytes([0x24; 32]).to_string_full(),
                expected_id.to_string_full()
            )
            .into_bytes(),
        );

        let state = state_from_commit(&commit, tree, vec![], false).unwrap();

        assert_eq!(state.change_id, expected_id);
        assert_eq!(state.status, Status::Published);
        assert_eq!(state.confidence, Some(0.875));
        assert_eq!(state.attribution.principal.name, b"Luke");
        assert_eq!(state.attribution.principal.email, b"luke@example.com");
        let agent = state.attribution.agent.expect("note agent preserved");
        assert_eq!(agent.provider, "anthropic");
        assert_eq!(agent.model, "claude-opus");
    }

    /// #564 step 1: a re-imported commit must round-trip every git-fidelity
    /// field — distinct committer identity, both timezone offsets, the
    /// verbatim message, and the extra headers in order (gpgsig kept inline at
    /// its captured position) — so the commit is byte-reconstructable later
    /// (#566) without the git mirror.
    #[test]
    fn state_from_commit_preserves_git_fidelity_fields() {
        let tree = empty_tree_hash();

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

        let state = state_from_commit(&commit, tree, vec![], false).unwrap();

        let committer = state.committer.expect("committer preserved");
        assert_eq!(committer.name, b"Committer");
        assert_eq!(committer.email, b"committer@example.com");
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

    #[test]
    fn descriptor_preserves_non_root_exported_source_state_until_full_parent_validation() {
        let tree = empty_tree_hash();
        let parent = StateId::from_bytes([0x31; 32]);
        let source = State::new(
            tree,
            vec![parent],
            Attribution::human(Principal::new("Exported", "exported@example.com")),
        )
        .with_change_id(ChangeId::from_bytes([0x52; 16]))
        .with_intent("exported child");
        let mut commit = make_commit(
            "34".repeat(20).as_str(),
            vec!["12".repeat(20)],
            "exported child\n",
        );
        commit.heddle_note = Some(
            HeddleNote::from_state(&source)
                .to_json_bytes()
                .expect("encode canonical note"),
        );

        let descriptor = descriptor_state_from_commit(&commit, tree, false)
            .expect("lazy descriptor preserves the portable source state");
        assert_eq!(descriptor, source);
        assert!(
            state_from_commit(&commit, tree, Vec::new(), false).is_err(),
            "full import must still reject a source state against the wrong parent graph"
        );
        assert_eq!(
            state_from_commit(&commit, tree, vec![parent], false)
                .expect("full import validates the real parent graph"),
            source
        );
    }

    #[test]
    fn rewritten_note_uses_actual_ordered_git_parents() {
        let tree = empty_tree_hash();
        let original_parent = StateId::from_bytes([0x31; 32]);
        let mapped_parents = vec![
            StateId::from_bytes([0x41; 32]),
            StateId::from_bytes([0x42; 32]),
        ];
        let source = State::new(
            tree,
            vec![original_parent],
            Attribution::human(Principal::new("Exported", "exported@example.com")),
        )
        .with_change_id(ChangeId::from_bytes([0x52; 16]))
        .with_status(Status::Published);
        let mut note = HeddleNote::from_state(&source);
        note.parents_rewritten = true;
        let mut commit = make_commit(
            "34".repeat(20).as_str(),
            vec!["12".repeat(20), "23".repeat(20)],
            "exported merge\n",
        );
        commit.heddle_note = Some(note.to_json_bytes().expect("canonical note"));

        let converted = state_from_commit(&commit, tree, mapped_parents.clone(), false)
            .expect("rewritten note converts");
        assert_eq!(converted.parents, mapped_parents);
        assert_eq!(converted.status, Status::Published);
        assert_eq!(converted.change_id, source.change_id);
        assert_ne!(converted.id(), source.id());
    }

    #[test]
    fn descendant_note_rebuilds_only_for_certified_parent_rewrites() {
        let tree = empty_tree_hash();
        let old_parent = StateId::from_bytes([0x51; 32]);
        let new_parent = StateId::from_bytes([0x61; 32]);
        let source = State::new(
            tree,
            vec![old_parent],
            Attribution::human(Principal::new("Exported", "exported@example.com")),
        )
        .with_change_id(ChangeId::from_bytes([0x72; 16]));
        let mut commit = make_commit("56".repeat(20).as_str(), vec!["34".repeat(20)], "child\n");
        commit.heddle_note = Some(
            HeddleNote::from_state(&source)
                .to_json_bytes()
                .expect("canonical note"),
        );

        assert!(state_from_commit(&commit, tree, vec![new_parent], false).is_err());
        let converted =
            state_from_commit_with_rewrites(&commit, tree, vec![new_parent], false, |id| {
                Ok((id == old_parent).then_some(new_parent))
            })
            .expect("certified rewrite");
        assert_eq!(converted.parents, vec![new_parent]);
        assert_ne!(converted.id(), source.id());
    }
}
