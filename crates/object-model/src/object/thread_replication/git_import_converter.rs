// SPDX-License-Identifier: Apache-2.0
//! One canonical Git-commit-to-State converter for local and hosted import.
//! Git tree/blob conversion supplies the mapped tree; this converter owns
//! identity, ordered parents, metadata, and embedded Heddle note repair.

use chrono::{DateTime, TimeZone, Utc};

use super::git_import_graph::{GitObjectFormat, GitObjectId};
use crate::{
    error::{HeddleError, Result},
    object::{
        Agent, Attribution, ChangeId, ChangeLineage, ChangeLineageKind, ContentHash, HeddleNote,
        Principal, State, StateId, Status,
    },
};

#[derive(Clone, Debug)]
pub struct GitImportSignature {
    pub name: Vec<u8>,
    pub email: Vec<u8>,
    pub time: DateTime<Utc>,
    pub tz_offset: i32,
}

#[derive(Clone, Debug)]
pub struct GitImportCommit<'a> {
    pub oid: &'a GitObjectId,
    pub author: GitImportSignature,
    pub committer: GitImportSignature,
    pub message: &'a [u8],
    pub extra_headers: &'a [(Vec<u8>, Vec<u8>)],
    pub heddle_note: Option<&'a [u8]>,
}

pub struct GitImportRawCommit<'a> {
    pub oid: &'a GitObjectId,
    pub object_format: GitObjectFormat,
    pub raw_commit: &'a [u8],
    pub heddle_note: Option<&'a [u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitImportParentPolicy {
    Validate,
    PreserveEmbedded,
}

pub struct GitImportGraph;

impl GitImportGraph {
    /// Parse a raw Git commit and feed the same converter as local ingest.
    /// Invalid identities or timestamps fail.
    pub fn convert_raw_commit(
        commit: GitImportRawCommit<'_>,
        tree: ContentHash,
        parents: Vec<StateId>,
        git_lossy: bool,
        rewritten_parent: impl Fn(StateId) -> Result<Option<StateId>>,
    ) -> Result<State> {
        let actual_oid = sley_core::object_id_for_bytes(
            commit.object_format.sley(),
            "commit",
            commit.raw_commit,
        )
        .map_err(|error| invalid(format!("hash Git commit: {error}")))?;
        let claimed_oid = match commit.oid {
            GitObjectId::Sha1(bytes) => bytes.as_slice(),
            GitObjectId::Sha256(bytes) => bytes.as_slice(),
        };
        if actual_oid.as_bytes() != claimed_oid {
            return Err(invalid("raw Git commit does not match its OID"));
        }
        let parsed = sley_object::Commit::parse_ref(commit.object_format.sley(), commit.raw_commit)
            .map_err(|error| invalid(format!("invalid Git commit: {error}")))?;
        let author = parse_signature(parsed.author)?;
        let committer = parse_signature(parsed.committer)?;
        let extra_headers = crate::object::parse_commit_extension_headers(commit.raw_commit);
        Self::convert_commit(
            GitImportCommit {
                oid: commit.oid,
                author,
                committer,
                message: parsed.message,
                extra_headers: &extra_headers,
                heddle_note: commit.heddle_note,
            },
            tree,
            parents,
            git_lossy,
            GitImportParentPolicy::Validate,
            rewritten_parent,
        )
    }

    /// Convert one Git commit after its tree and ordered parents are mapped.
    /// A rewritten embedded note can change State identity; descendants may
    /// refer to that original ID only through the durable rewrite lookup.
    pub fn convert_commit(
        commit: GitImportCommit<'_>,
        tree: ContentHash,
        parents: Vec<StateId>,
        git_lossy: bool,
        parent_policy: GitImportParentPolicy,
        rewritten_parent: impl Fn(StateId) -> Result<Option<StateId>>,
    ) -> Result<State> {
        let message = String::from_utf8_lossy(commit.message);
        let note = commit
            .heddle_note
            .map(HeddleNote::from_json_bytes)
            .transpose()
            .map_err(|error| invalid(format!("invalid Heddle note: {error}")))?;
        if let Some(note) = note.as_ref()
            && let Some(mut source_state) = note.source_state.clone()
        {
            let original = source_state.id();
            let parent_mismatch =
                parent_policy == GitImportParentPolicy::Validate && source_state.parents != parents;
            let explained_rewrites = if parent_mismatch && !note.parents_rewritten {
                if source_state.parents.len() != parents.len() {
                    false
                } else {
                    let mut explained = true;
                    for (before, after) in source_state.parents.iter().zip(&parents) {
                        if before != after && rewritten_parent(*before)? != Some(*after) {
                            explained = false;
                            break;
                        }
                    }
                    explained
                }
            } else {
                false
            };
            if original.to_string_full() != note.state_id
                || source_state.change_id.to_string_full() != note.change_id
                || source_state.tree != tree
                || (parent_mismatch && !note.parents_rewritten && !explained_rewrites)
            {
                return Err(invalid(
                    "embedded Heddle State differs from note, tree, or Git parents",
                ));
            }
            if parent_mismatch {
                source_state.parents = parents;
                source_state.state_id = source_state.id();
            } else {
                source_state.state_id = original;
            }
            return Ok(source_state);
        }

        let identity = resolve_identity(commit.oid, &message, note.as_ref())?;
        let attribution = parse_git_attribution(&commit.author, &message, note.as_ref());
        let mut state = State::new(tree, parents, attribution)
            .with_change_id(identity)
            .with_timestamp(commit.committer.time)
            .with_authored_at(commit.author.time)
            .with_intent(message.lines().next().unwrap_or("").trim().to_string())
            .with_committer(Principal::new(
                &commit.committer.name,
                &commit.committer.email,
            ))
            .with_tz_offsets(commit.author.tz_offset, commit.committer.tz_offset)
            .with_raw_message(commit.message)
            .with_git_lossy(git_lossy)
            .with_extra_headers(commit.extra_headers.to_vec())
            .with_status(note_status(note.as_ref()));
        if let Some(confidence) = note.as_ref().and_then(|value| value.confidence) {
            state = state.with_confidence(confidence);
        }
        if let Some(note) = note {
            let source_state = StateId::parse(&note.state_id)
                .map_err(|error| invalid(format!("invalid Heddle note StateId: {error}")))?;
            if state.id() != source_state {
                let source_change = state.change_id;
                state = state.with_lineage(vec![ChangeLineage {
                    kind: ChangeLineageKind::GitProjection,
                    source_change,
                    source_state,
                }]);
            }
        }
        Ok(state)
    }
}

fn parse_signature(raw: &[u8]) -> Result<GitImportSignature> {
    let value = sley_core::Signature::from_ident_line(raw)
        .ok_or_else(|| invalid("invalid Git author or committer signature"))?;
    let time = Utc
        .timestamp_opt(value.time.seconds, 0)
        .single()
        .ok_or_else(|| invalid("Git signature timestamp is out of range"))?;
    Ok(GitImportSignature {
        name: value.name.as_bytes().to_vec(),
        email: value.email.as_bytes().to_vec(),
        time,
        tz_offset: i32::from(value.time.timezone_offset_minutes) * 60,
    })
}

fn invalid(message: impl Into<String>) -> HeddleError {
    HeddleError::InvalidObject(message.into())
}

fn resolve_identity(
    oid: &GitObjectId,
    message: &str,
    note: Option<&HeddleNote>,
) -> Result<ChangeId> {
    if let Some(note) = note {
        return ChangeId::parse(&note.change_id)
            .map_err(|error| invalid(format!("invalid Heddle note ChangeId: {error}")));
    }
    if let Some(change_id) = parse_trailers(message).get("Heddle-Change-Id") {
        return ChangeId::parse(change_id)
            .map_err(|error| invalid(format!("invalid Heddle-Change-Id trailer: {error}")));
    }
    let bytes = match oid {
        GitObjectId::Sha1(bytes) => bytes.as_slice(),
        GitObjectId::Sha256(bytes) => bytes.as_slice(),
    };
    let digest = ContentHash::compute_typed("git-change", bytes);
    let mut identity = [0; 16];
    identity.copy_from_slice(&digest.as_bytes()[..16]);
    Ok(ChangeId::from_bytes(identity))
}

fn note_status(note: Option<&HeddleNote>) -> Status {
    match note.map(|value| value.status.as_str()) {
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
            if key.starts_with("Heddle-") {
                trailers.insert(key.to_string(), line[pos + 1..].trim().to_string());
            }
        } else if !line.trim().is_empty() {
            break;
        }
    }
    trailers
}

pub fn parse_git_attribution(
    author: &GitImportSignature,
    message: &str,
    note: Option<&HeddleNote>,
) -> Attribution {
    let principal = note
        .and_then(|value| value.attribution.as_ref())
        .map(|attribution| {
            Principal::new(
                attribution.principal_name.clone(),
                attribution.principal_email.clone(),
            )
        })
        .unwrap_or_else(|| Principal::new(&author.name, &author.email));
    if let Some(agent) = note
        .and_then(|value| value.attribution.as_ref())
        .and_then(|attribution| attribution.agent.as_ref())
        .or_else(|| note.and_then(|value| value.agent.as_ref()))
        .cloned()
        .or_else(|| detect_agent_in_message(message))
    {
        Attribution::with_agent(principal, agent)
    } else {
        Attribution::human(principal)
    }
}

fn detect_agent_in_message(message: &str) -> Option<Agent> {
    for line in message.lines().rev() {
        let lower = line.to_ascii_lowercase();
        if !lower.starts_with("co-authored-by:") {
            continue;
        }
        let rest = line["co-authored-by:".len()..].trim();
        let (name, email) = match (rest.rfind('<'), rest.rfind('>')) {
            (Some(start), Some(end)) if end > start => {
                (rest[..start].trim(), rest[start + 1..end].trim())
            }
            _ => (rest, ""),
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
    }
    None
}

fn best_model_from(name: &str, fallback: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else if trimmed.chars().any(|ch| ch.is_ascii_digit() || ch == '-') {
        trimmed.to_string()
    } else {
        fallback.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Tree;

    #[test]
    fn embedded_integration_rejects_uncertified_parent_identity_count_and_order() {
        let tree = Tree::new().hash();
        let a = StateId::from_bytes([1; 32]);
        let b = StateId::from_bytes([2; 32]);
        let forged = StateId::from_bytes([3; 32]);
        let source = State::new(
            tree,
            vec![a, b],
            Attribution::human(Principal::new("Test", "test@example.com")),
        )
        .with_intent("automatic integration merge");
        let bytes = HeddleNote::from_state(&source)
            .to_json_bytes()
            .expect("note");
        let oid = GitObjectId::Sha1([4; 20]);
        let convert = |parents, rewrite| {
            let signature = GitImportSignature {
                name: b"Test".to_vec(),
                email: b"test@example.com".to_vec(),
                time: DateTime::UNIX_EPOCH,
                tz_offset: 0,
            };
            GitImportGraph::convert_commit(
                GitImportCommit {
                    oid: &oid,
                    author: signature.clone(),
                    committer: signature,
                    message: b"automatic integration merge",
                    extra_headers: &[],
                    heddle_note: Some(&bytes),
                },
                tree,
                parents,
                false,
                GitImportParentPolicy::Validate,
                |id| Ok((rewrite && id == a).then_some(forged)),
            )
        };
        assert!(
            convert(vec![forged, b], false).is_err(),
            "identity requires a certified rewrite"
        );
        assert!(
            convert(vec![a], true).is_err(),
            "a rewrite cannot explain a dropped parent"
        );
        assert!(
            convert(vec![b, a], true).is_err(),
            "a rewrite cannot explain reordered parents"
        );
        assert!(
            convert(vec![a, forged], true).is_err(),
            "a rewrite of a cannot explain forged b"
        );
        let repaired = convert(vec![forged, b], true).expect("certified identity rewrite");
        assert_eq!(repaired.parents, vec![forged, b]);
        assert_ne!(repaired.id(), source.id());
    }

    #[test]
    fn typed_local_and_raw_conversion_are_byte_identical() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor Test <test@example.com> 0 +0000\ncommitter Test <test@example.com> 0 +0000\n\nmessage\n";
        let oid = sley_core::object_id_for_bytes(sley_core::ObjectFormat::Sha1, "commit", raw)
            .expect("OID");
        let oid = GitObjectId::Sha1(oid.as_bytes().try_into().expect("SHA-1"));
        let signature = GitImportSignature {
            name: b"Test".to_vec(),
            email: b"test@example.com".to_vec(),
            time: DateTime::UNIX_EPOCH,
            tz_offset: 0,
        };
        let tree = Tree::new().hash();
        let typed = GitImportGraph::convert_commit(
            GitImportCommit {
                oid: &oid,
                author: signature.clone(),
                committer: signature,
                message: b"message\n",
                extra_headers: &[],
                heddle_note: None,
            },
            tree,
            Vec::new(),
            false,
            GitImportParentPolicy::Validate,
            |_| Ok(None),
        )
        .expect("typed local State");
        let raw_state = GitImportGraph::convert_raw_commit(
            GitImportRawCommit {
                oid: &oid,
                object_format: GitObjectFormat::Sha1,
                raw_commit: raw,
                heddle_note: None,
            },
            tree,
            Vec::new(),
            false,
            |_| Ok(None),
        )
        .expect("raw State");
        assert_eq!(
            typed.encode_current_msgpack().expect("typed bytes"),
            raw_state.encode_current_msgpack().expect("raw bytes")
        );
    }
}
