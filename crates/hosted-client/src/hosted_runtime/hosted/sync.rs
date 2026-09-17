use std::time::Duration;

use api::heddle::api::v1alpha1::{PullReady, RefEntry as ProtoRefEntry};
use objects::{
    object::{
        AnnotationStatus, ContextBlob, ContextTarget, Discussion, DiscussionError, DiscussionsBlob,
        StateAttachmentBody, StateAttachmentKind, StateId,
    },
    store::ObjectStore,
};
use repo::{Repository, ThreadManager};
use wire::{ProtocolError, RefEntry, RefKind};

const PULL_BOOTSTRAP_LINE_PREFIX: &str = "heddle-pull-bootstrap-v1:";
const PULL_REFS_LINE_PREFIX: &str = "heddle-pull-refs-v1:";
type PullBootstrapPayload = (
    bool,
    Vec<Discussion>,
    bool,
    Vec<(ContextTarget, ContextBlob)>,
);

#[derive(Debug, Clone)]
pub struct PullBootstrapMetadata {
    pub discussions_from_pack: bool,
    pub discussions: Vec<Discussion>,
    pub context_from_pack: bool,
    pub context: Vec<(ContextTarget, ContextBlob)>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPullBootstrapMetadata {
    /// Packed or inline bootstrap discussions. `None` means the server
    /// advertised `discussions_from_pack` and this client cannot consume the
    /// attachment (missing, wrong kind, or version skew) — callers pass that
    /// to `pull_discussions` so it ObserveCollaboration-falls-back instead of
    /// treating an empty vec as "no discussions". v2 source-only bootstrap is
    /// an empty inline fold; pull treats that empty slice the same way.
    pub discussions: Option<Vec<Discussion>>,
    /// Human-facing reason when [`Self::discussions`] is `None` because the
    /// packed attachment was unconsumable. Absent on the inline-bootstrap path
    /// and when the pack decoded.
    pub discussions_pack_fallback: Option<String>,
    /// Packed or inline bootstrap context. `None` means the server advertised
    /// `context_from_pack` and this client cannot consume the attachment, so
    /// callers fall back to `ListContext`.
    pub context: Option<Vec<(ContextTarget, ContextBlob)>>,
    /// Human-facing reason when [`Self::context`] is `None`. Absent on the
    /// inline-bootstrap path and when the packed attachment decoded.
    pub context_pack_fallback: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PullBootstrapRefs {
    pub head_thread: Option<String>,
    pub refs: Vec<HostedRefEntry>,
}

impl PullBootstrapMetadata {
    pub fn resolve(
        &self,
        repo: &Repository,
        state_id: Option<StateId>,
    ) -> Result<ResolvedPullBootstrapMetadata, ProtocolError> {
        let state_id = state_id.ok_or_else(|| {
            ProtocolError::InvalidState("pull bootstrap is missing its final state".to_string())
        })?;
        let (discussions, discussions_pack_fallback) = if self.discussions_from_pack {
            match discussions_from_pull_pack(repo, state_id)? {
                PackedDiscussions::Ready(discussions) => (Some(discussions), None),
                PackedDiscussions::Unconsumable(reason) => {
                    let warning = format!(
                        "pull bootstrap advertised packed discussions but this client cannot consume them ({reason}); falling back to ObserveCollaboration"
                    );
                    (None, Some(warning))
                }
            }
        } else {
            (Some(self.discussions.clone()), None)
        };
        let (context, context_pack_fallback) = if self.context_from_pack {
            match context_from_pull_pack(repo, state_id)? {
                PackedContext::Ready(context) => (Some(context), None),
                PackedContext::Unconsumable(reason) => {
                    let warning = format!(
                        "pull bootstrap advertised packed context but this client cannot consume it ({reason}); falling back to ObserveCollaboration"
                    );
                    (None, Some(warning))
                }
            }
        } else {
            (Some(self.context.clone()), None)
        };
        Ok(ResolvedPullBootstrapMetadata {
            discussions,
            discussions_pack_fallback,
            context,
            context_pack_fallback,
        })
    }
}

/// Inline source-only bootstrap. v2 Fetch does not fold discussions/context;
/// pull/clone still require this header so they can materialize source.
pub fn encode_empty_pull_bootstrap(state: StateId) -> Result<Vec<u8>, ProtocolError> {
    let payload = rmp_serde::to_vec_named(&(
        false,
        Vec::<Discussion>::new(),
        false,
        Vec::<(ContextTarget, ContextBlob)>::new(),
    ))
    .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    Ok(format!(
        "{PULL_BOOTSTRAP_LINE_PREFIX}{}\t{}\n",
        hex::encode(payload),
        state.to_string_full()
    )
    .into_bytes())
}

pub fn decode_pull_bootstrap(
    checkpoint: &[u8],
) -> Result<Option<PullBootstrapMetadata>, ProtocolError> {
    let checkpoint = std::str::from_utf8(checkpoint)
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    let Some(payload) = checkpoint
        .lines()
        .find_map(|line| line.strip_prefix(PULL_BOOTSTRAP_LINE_PREFIX))
    else {
        return Ok(None);
    };
    let payload = payload
        .split_once('\t')
        .map(|(payload, _)| payload)
        .ok_or_else(|| {
            ProtocolError::InvalidState("decode pull bootstrap: missing sentinel state".to_string())
        })?;
    let payload = hex::decode(payload)
        .map_err(|error| ProtocolError::InvalidState(format!("decode pull bootstrap: {error}")))?;
    let (discussions_from_pack, discussions, context_from_pack, context): PullBootstrapPayload =
        rmp_serde::from_slice(&payload).map_err(|error| {
            ProtocolError::InvalidState(format!("decode pull bootstrap: {error}"))
        })?;
    Ok(Some(PullBootstrapMetadata {
        discussions_from_pack,
        discussions,
        context_from_pack,
        context,
    }))
}

/// The hex-msgpack `heddle-pull-refs-v1:` checkpoint fold is a removed
/// side channel. Clone/pull bootstrap refs are heddle-api `RefEntry`
/// on `PullReady.refs`, with ListRefs as the empty-refs fallback.
pub fn reject_legacy_pull_refs_fold(checkpoint: &[u8]) -> Result<(), ProtocolError> {
    let Ok(text) = std::str::from_utf8(checkpoint) else {
        return Ok(());
    };
    if text
        .lines()
        .any(|line| line.starts_with(PULL_REFS_LINE_PREFIX))
    {
        return Err(ProtocolError::InvalidState(
            "server advertised removed pull-refs side channel; RefEntry on the pull path is the contract"
                .to_string(),
        ));
    }
    Ok(())
}

/// Map a heddle-api `RefEntry` (ListRefs item / PullReady.refs) into the
/// hosted clone record. Empty `thread_id` stays absent so ListRefs can
/// still supply it; non-user-threads drop a folded id.
pub fn hosted_ref_from_api(entry: &ProtoRefEntry) -> Result<HostedRefEntry, ProtocolError> {
    let state_id = super::helpers::parse_proto_state_id(entry.state_id.clone())?
        .ok_or_else(|| ProtocolError::InvalidState("ref is missing its state ID".to_string()))?;
    let thread_id = (!entry.thread_id.is_empty()).then(|| entry.thread_id.clone());
    Ok(HostedRefEntry::from_advertised(
        entry.name.clone(),
        state_id,
        entry.is_thread,
        entry.revision_address.clone(),
        thread_id,
    ))
}

/// Prefer `PullReady.refs` when the server advertised them. `None` means
/// an older peer left the field empty — callers ListRefs. The removed
/// hex-msgpack fold still fails closed.
pub fn pull_refs_from_ready(ready: &PullReady) -> Result<Option<PullBootstrapRefs>, ProtocolError> {
    let checkpoint = ready
        .transfer
        .as_ref()
        .map(|transfer| transfer.checkpoint.as_slice())
        .unwrap_or_default();
    reject_legacy_pull_refs_fold(checkpoint)?;
    if ready.refs.is_empty() {
        return Ok(None);
    }
    if ready.refs.len() > api::MAX_PAGE_SIZE as usize {
        return Err(ProtocolError::InvalidState(format!(
            "PullReady refs exceed MAX_PAGE_SIZE ({})",
            api::MAX_PAGE_SIZE
        )));
    }
    let refs = ready
        .refs
        .iter()
        .map(hosted_ref_from_api)
        .collect::<Result<Vec<_>, _>>()?;
    let head_thread = {
        let trimmed = ready.head_thread.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };
    Ok(Some(PullBootstrapRefs { head_thread, refs }))
}

enum PackedDiscussions {
    Ready(Vec<Discussion>),
    /// Server advertised a pack optimization this client cannot consume.
    /// Clone/pull must ListByState instead of exiting 76.
    Unconsumable(String),
}

fn discussions_from_pull_pack(
    repo: &Repository,
    state_id: StateId,
) -> Result<PackedDiscussions, ProtocolError> {
    let Some(attachment) =
        repo.latest_state_attachment(&state_id, StateAttachmentKind::Discussions)?
    else {
        return Ok(PackedDiscussions::Unconsumable(
            "the attachment is missing".to_string(),
        ));
    };
    let StateAttachmentBody::Discussions(hash) = attachment.body else {
        return Ok(PackedDiscussions::Unconsumable(
            "the attachment has the wrong kind".to_string(),
        ));
    };
    let blob = repo.store().get_blob(&hash)?.ok_or_else(|| {
        ProtocolError::InvalidState(format!(
            "packed discussions attachment references missing blob {hash}"
        ))
    })?;
    match DiscussionsBlob::decode(blob.content()) {
        Ok(blob) => Ok(PackedDiscussions::Ready(blob.discussions)),
        Err(DiscussionError::UnsupportedVersion(version)) => Ok(PackedDiscussions::Unconsumable(
            format!("unsupported blob version {version}"),
        )),
        Err(DiscussionError::Encoding(error)) if named_map_version_skew(blob.content()) => {
            Ok(PackedDiscussions::Unconsumable(format!(
                "blob encoding is not this client's DiscussionsBlob v1: {error}"
            )))
        }
        Err(error) => Err(ProtocolError::InvalidState(error.to_string())),
    }
}

/// weft's v2 root is a named map with a version that is not this client's
/// positional `DiscussionsBlob` v1. Peek enough of that map to identify skew
/// (`format_version` / `version` ≠ 1). An empty or keyless map is corruption.
fn named_map_version_skew(bytes: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct Peek {
        #[serde(default)]
        format_version: Option<u8>,
        #[serde(default)]
        version: Option<u8>,
    }
    let Ok(peek) = rmp_serde::from_slice::<Peek>(bytes) else {
        return false;
    };
    peek.format_version
        .or(peek.version)
        .is_some_and(|version| version != DiscussionsBlob::FORMAT_VERSION)
}

enum PackedContext {
    Ready(Vec<(ContextTarget, ContextBlob)>),
    /// Server advertised a pack optimization this client cannot consume.
    /// Clone/pull must ListContext instead of exiting 76.
    Unconsumable(String),
}

fn context_from_pull_pack(
    repo: &Repository,
    state_id: StateId,
) -> Result<PackedContext, ProtocolError> {
    let Some(attachment) = repo.latest_state_attachment(&state_id, StateAttachmentKind::Context)?
    else {
        return Ok(PackedContext::Unconsumable(
            "the attachment is missing".to_string(),
        ));
    };
    let StateAttachmentBody::Context(root) = attachment.body else {
        return Ok(PackedContext::Unconsumable(
            "the attachment has the wrong kind".to_string(),
        ));
    };
    if repo.store().get_tree(&root)?.is_none() {
        return Ok(PackedContext::Unconsumable(format!(
            "the context root {root} is missing"
        )));
    }
    let mut entries = repo
        .list_context_entries(&root, None)?
        .into_iter()
        .map(|entry| (entry.target, entry.blob))
        .collect::<Vec<_>>();
    for (_, blob) in &mut entries {
        blob.annotations
            .retain(|annotation| annotation.status == AnnotationStatus::Active);
    }
    entries.retain(|(_, blob)| !blob.annotations.is_empty());
    Ok(PackedContext::Ready(entries))
}

#[derive(Debug, Clone, Default)]
pub struct PullObjectMix {
    pub blobs: usize,
    pub trees: usize,
    pub states: usize,
    pub actions: usize,
    pub annotated_tags: usize,
    pub redactions: usize,
    pub purges: usize,
    pub state_visibilities: usize,
    pub state_attachments: usize,
    pub key_bindings: usize,
}

impl PullObjectMix {
    pub fn total(&self) -> usize {
        self.blobs
            + self.trees
            + self.states
            + self.actions
            + self.annotated_tags
            + self.redactions
            + self.purges
            + self.state_visibilities
            + self.state_attachments
            + self.key_bindings
    }
}

#[derive(Debug, Clone)]
pub struct HostedRefEntry {
    pub name: String,
    pub state_id: StateId,
    pub kind: RefKind,
    pub revision_address: String,
    pub thread_id: Option<String>,
}

impl HostedRefEntry {
    pub fn from_advertised(
        name: String,
        state_id: StateId,
        advertised_as_thread: bool,
        revision_address: String,
        thread_id: Option<String>,
    ) -> Self {
        let kind = RefKind::from_advertised_name(&name, advertised_as_thread);
        let thread_id = if kind.is_user_thread() {
            thread_id
        } else {
            None
        };
        Self {
            name,
            state_id,
            kind,
            revision_address,
            thread_id,
        }
    }

    pub fn is_user_thread(&self) -> bool {
        self.kind.is_user_thread()
    }

    pub fn is_marker(&self) -> bool {
        self.kind.is_marker()
    }

    pub fn to_wire_entry(&self) -> RefEntry {
        RefEntry {
            name: self.name.clone(),
            state_id: self.state_id,
            kind: self.kind,
        }
    }
}

/// Advertised ListRefs `thread_id` for a user thread, if present and non-empty.
pub fn advertised_user_thread_id<'a>(
    remote_refs: &'a [HostedRefEntry],
    track_name: &str,
) -> Option<&'a str> {
    remote_refs
        .iter()
        .find(|entry| entry.is_user_thread() && entry.name == track_name)
        .and_then(|entry| entry.thread_id.as_deref())
        .filter(|id| !id.is_empty())
}

/// Persist the ListRefs-advertised stable id as the local thread identity.
///
/// Clone uses this when GetThread is missing; tests drive the same helper so
/// a later push cannot mint a different UUIDv7.
pub fn persist_advertised_thread_identity(
    repo: &Repository,
    remote_refs: &[HostedRefEntry],
    track_name: &str,
    final_state: &StateId,
) -> objects::error::Result<()> {
    let Some(stable_id) = advertised_user_thread_id(remote_refs, track_name) else {
        return Ok(());
    };
    ThreadManager::new(repo.heddle_dir()).adopt_stable_identity_for_thread(
        repo,
        track_name,
        stable_id,
        *final_state,
    )?;
    Ok(())
}

/// Persist from advertised pull refs when they carry an id; otherwise
/// from a live ListRefs advertisement. Empty `thread_id` stays absent.
pub fn persist_advertised_thread_identity_with_live_fallback(
    repo: &Repository,
    advertised_refs: &[HostedRefEntry],
    live_refs: Option<&[HostedRefEntry]>,
    track_name: &str,
    final_state: &StateId,
) -> objects::error::Result<()> {
    if advertised_user_thread_id(advertised_refs, track_name).is_some() {
        return persist_advertised_thread_identity(repo, advertised_refs, track_name, final_state);
    }
    persist_advertised_thread_identity(
        repo,
        live_refs.unwrap_or(advertised_refs),
        track_name,
        final_state,
    )
}

#[derive(Debug, Clone, Default)]
pub struct PullProfile {
    pub ready_wait: Duration,
    pub receive_and_apply: Duration,
    pub decode: Duration,
    pub store_receive_object: Duration,
    pub metadata_sync: Duration,
    pub pack_decode_apply: Duration,
    pub raw_decode_apply: Duration,
    pub pack_decode: Duration,
    pub raw_decode: Duration,
    pub bytes_received: usize,
    pub pack_bytes_received: usize,
    pub raw_bytes_received: usize,
    pub objects_received: usize,
    pub object_mix: PullObjectMix,
}

#[derive(Debug, Clone, Default)]
pub struct PushProfile {
    pub remote_ref_discovery: Duration,
    pub closure_planning: Duration,
    pub request_setup: Duration,
    pub ready_wait: Duration,
    pub pack_and_send: Duration,
    pub sidecars_and_git_send: Duration,
    pub request_finish: Duration,
    pub complete_wait: Duration,
    pub metadata_sync: Duration,
    pub total: Duration,
    pub objects_advertised: usize,
    pub objects_wanted: usize,
    pub pack_objects: usize,
    pub sidecar_objects: usize,
    pub reused_pack: usize,
    pub reused_pack_copy: Duration,
}

/// Compare-and-set precondition for a native push target.
///
/// Callers that retain the head from a successful push or pull can provide it
/// here and avoid a separate `ListRefs` request before every push.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ExpectedRemoteHead {
    Missing,
    State(StateId),
}

#[cfg(test)]
mod pull_bootstrap_tests {
    use api::heddle::api::v1alpha1::{
        PullReady, RefEntry as ProtoRefEntry, StateId as ProtoStateId, TransferCheckpoint,
    };
    use chrono::Utc;
    use objects::{
        object::{
            Annotation, AnnotationKind, AnnotationScope, Attribution, Blob, ContentHash,
            ContextBlob, ContextTarget, Discussion, DiscussionResolution, DiscussionTurn,
            DiscussionsBlob, Principal, StateAttachment, StateAttachmentBody, SymbolAnchor, Tree,
            TreeEntry, VisibilityTier,
        },
        store::ObjectStore,
    };
    use repo::RevisionAddress;
    use tempfile::TempDir;
    use wire::{AdvertisedRef, RefKind};

    use super::*;

    #[test]
    fn empty_source_bootstrap_round_trips() {
        let state = StateId::from_bytes([3; 32]);
        let checkpoint = encode_empty_pull_bootstrap(state).expect("encode");
        let decoded = decode_pull_bootstrap(&checkpoint)
            .expect("decode")
            .expect("header present");
        assert!(!decoded.discussions_from_pack);
        assert!(!decoded.context_from_pack);
        assert!(decoded.discussions.is_empty());
        assert!(decoded.context.is_empty());
    }

    #[test]
    fn bootstrap_decoder_accepts_structured_empty_metadata() {
        let payload = rmp_serde::to_vec_named(&(
            true,
            Vec::<Discussion>::new(),
            true,
            Vec::<(ContextTarget, ContextBlob)>::new(),
        ))
        .expect("encode server-compatible pull bootstrap");
        let checkpoint = format!(
            "heddle-markers-v1\nrelease\t{}\n{PULL_BOOTSTRAP_LINE_PREFIX}{}\t{}\n",
            StateId::from_bytes([1; 32]).to_string_full(),
            hex::encode(payload),
            StateId::from_bytes([0; 32]).to_string_full()
        );

        let decoded = decode_pull_bootstrap(checkpoint.as_bytes())
            .expect("decode pull bootstrap")
            .expect("new-server header must select bootstrap metadata");
        assert!(decoded.discussions.is_empty());
        assert!(decoded.context.is_empty());
        assert!(decoded.discussions_from_pack);
        assert!(decoded.context_from_pack);
    }

    fn proto_state(state: StateId) -> ProtoStateId {
        ProtoStateId {
            value: state.as_bytes().to_vec(),
        }
    }

    fn proto_ref(
        name: impl Into<String>,
        state: StateId,
        is_thread: bool,
        revision_address: impl Into<String>,
        thread_id: impl Into<String>,
    ) -> ProtoRefEntry {
        ProtoRefEntry {
            name: name.into(),
            state_id: Some(proto_state(state)),
            is_thread,
            revision_address: revision_address.into(),
            thread_id: thread_id.into(),
        }
    }

    #[test]
    fn malformed_advertised_bootstrap_fails_loud() {
        let error = decode_pull_bootstrap(
            format!(
                "heddle-markers-v1\nheddle-pull-bootstrap-v1:not-hex\t{}\n",
                StateId::from_bytes([0; 32]).to_string_full()
            )
            .as_bytes(),
        )
        .expect_err("advertised but malformed metadata must not silently fall back");
        assert!(error.to_string().contains("decode pull bootstrap"));
    }

    #[test]
    fn api_ref_entry_adopts_thread_id() {
        let main = StateId::from_bytes([7; 32]);
        let release = StateId::from_bytes([8; 32]);
        let change = objects::object::ChangeId::from_bytes([9; 16]);
        let frontier = objects::object::SyntheticFrontierName::new("main", change)
            .unwrap()
            .as_name();
        let hosted_id = "019f0000-aaaa-7bbb-8ccc-ddddeeeeffff";
        let refs = [
            proto_ref("main", main, true, "git:0123456789abcdef", hosted_id),
            proto_ref(
                "empty",
                main,
                true,
                RevisionAddress::heddle(main).to_string(),
                "",
            ),
            proto_ref(
                "release",
                release,
                false,
                RevisionAddress::heddle(release).to_string(),
                hosted_id,
            ),
            proto_ref(frontier.clone(), main, true, "", hosted_id),
        ]
        .iter()
        .map(hosted_ref_from_api)
        .collect::<Result<Vec<_>, _>>()
        .expect("heddle-api RefEntry is the only clone-ref schema");
        assert_eq!(refs.len(), 4);
        assert_eq!(refs[0].name, "main");
        assert_eq!(refs[0].kind, RefKind::Thread);
        assert_eq!(refs[0].thread_id.as_deref(), Some(hosted_id));
        assert_eq!(refs[1].name, "empty");
        assert!(
            refs[1].thread_id.is_none(),
            "empty thread_id must stay absent so ListRefs can still supply it"
        );
        assert_eq!(refs[2].name, "release");
        assert_eq!(refs[2].kind, RefKind::Marker);
        assert!(
            refs[2].thread_id.is_none(),
            "markers must not adopt a thread_id"
        );
        assert_eq!(refs[3].name, frontier);
        assert_eq!(refs[3].kind, RefKind::SyntheticFrontierRoot);
        assert!(
            refs[3].thread_id.is_none(),
            "synthetic frontiers must not adopt a thread_id"
        );
    }

    #[test]
    fn legacy_pull_refs_fold_is_rejected() {
        let checkpoint = format!(
            "{PULL_REFS_LINE_PREFIX}deadbeef\t{}\n",
            StateId::from_bytes([0; 32]).to_string_full(),
        );
        let error = reject_legacy_pull_refs_fold(checkpoint.as_bytes())
            .expect_err("old hex-msgpack fold must fail closed");
        assert!(
            error.to_string().contains("removed pull-refs side channel"),
            "got {error}"
        );
        reject_legacy_pull_refs_fold(b"heddle-markers-v1\nrelease\tabc\n")
            .expect("other checkpoint lines are not the removed fold");
    }

    fn pull_ready_with_refs(
        refs: Vec<ProtoRefEntry>,
        head_thread: &str,
        checkpoint: &[u8],
    ) -> PullReady {
        PullReady {
            refs,
            head_thread: head_thread.to_string(),
            transfer: Some(TransferCheckpoint {
                checkpoint: checkpoint.to_vec(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn pull_ready_refs_are_preferred() {
        let main = StateId::from_bytes([7; 32]);
        let hosted_id = "019f0000-aaaa-7bbb-8ccc-ddddeeeeffff";
        let ready = pull_ready_with_refs(
            vec![proto_ref(
                "main",
                main,
                true,
                "git:0123456789abcdef",
                hosted_id,
            )],
            "main",
            b"",
        );
        let decoded = pull_refs_from_ready(&ready)
            .expect("PullReady.refs must map")
            .expect("non-empty refs are preferred over ListRefs");
        assert_eq!(decoded.head_thread.as_deref(), Some("main"));
        assert_eq!(decoded.refs.len(), 1);
        assert_eq!(decoded.refs[0].name, "main");
        assert_eq!(decoded.refs[0].thread_id.as_deref(), Some(hosted_id));
    }

    #[test]
    fn pull_ready_empty_refs_defer_to_list_refs() {
        let ready = pull_ready_with_refs(Vec::new(), "", b"");
        assert!(
            pull_refs_from_ready(&ready)
                .expect("empty refs are not an error")
                .is_none(),
            "empty PullReady.refs must leave room for ListRefs"
        );
    }

    #[test]
    fn pull_ready_rejects_fold_even_when_refs_present() {
        let main = StateId::from_bytes([7; 32]);
        let checkpoint = format!(
            "{PULL_REFS_LINE_PREFIX}deadbeef\t{}\n",
            StateId::from_bytes([0; 32]).to_string_full(),
        );
        let ready = pull_ready_with_refs(
            vec![proto_ref("main", main, true, "", "id")],
            "main",
            checkpoint.as_bytes(),
        );
        let error = pull_refs_from_ready(&ready)
            .expect_err("fold must fail closed even when PullReady.refs is set");
        assert!(
            error.to_string().contains("removed pull-refs side channel"),
            "got {error}"
        );
    }

    #[test]
    fn folded_synthetic_frontier_is_not_a_thread_or_marker() {
        let change = objects::object::ChangeId::from_bytes([9; 16]);
        let frontier = objects::object::SyntheticFrontierName::new("main", change)
            .unwrap()
            .as_name();
        let state = StateId::from_bytes([7; 32]);
        let decoded = hosted_ref_from_api(&proto_ref(frontier.clone(), state, true, "", ""))
            .expect("synthetic frontier RefEntry");
        assert_eq!(decoded.kind, RefKind::SyntheticFrontierRoot);
        assert!(!decoded.is_user_thread());
        assert!(!decoded.is_marker());
        match decoded.to_wire_entry().advertised() {
            Ok(AdvertisedRef::SyntheticFrontier(name)) => assert_eq!(name.as_name(), frontier),
            other => panic!("synthetic must stay synthetic, got {other:?}"),
        }
    }

    #[test]
    fn packed_bootstrap_resolves_the_same_discussion_and_context_domain_content() {
        let temp = TempDir::new().expect("temp repo");
        let repo = Repository::init_default(temp.path()).expect("init repo");
        std::fs::write(temp.path().join("lib.rs"), "pub fn run() {}\n").expect("write source");
        let snapshot = repo
            .snapshot(Some("seed".to_string()), None)
            .expect("snapshot");
        let principal = Principal::new("Ada", "ada@example.com");
        let discussion = Discussion {
            id: "discussion-1".to_string(),
            anchor: SymbolAnchor::new("lib.rs", "run"),
            opened_against_state: snapshot.state_id,
            opened_at: 1_700_000_000,
            thread_ref: None,
            turns: vec![DiscussionTurn {
                author: principal.clone(),
                body: "keep this invariant".to_string(),
                posted_at: 1_700_000_001,
                references: Vec::new(),
            }],
            resolution: DiscussionResolution::Open,
            body_changed_since_open: false,
            anchor_ambiguous: false,
            orphaned: false,
            visibility: VisibilityTier::Public,
            resolved_annotation_id: None,
        };
        let discussions_blob = DiscussionsBlob::new(vec![discussion.clone()]);
        let discussions_hash = repo
            .store()
            .put_blob(&Blob::from(
                discussions_blob.encode().expect("encode discussions"),
            ))
            .expect("store discussions");
        repo.put_state_attachment(&StateAttachment {
            state_id: snapshot.state_id,
            body: StateAttachmentBody::Discussions(discussions_hash),
            attribution: Attribution::human(principal.clone()),
            created_at: Utc::now(),
            supersedes: None,
        })
        .expect("attach discussions");

        let target = ContextTarget::file("lib.rs").expect("context target");
        let annotation = Annotation::new(
            AnnotationScope::File,
            AnnotationKind::Invariant,
            "run remains public".to_string(),
            vec!["contract".to_string()],
            principal.to_string(),
            1_700_000_002,
            None,
            Some(snapshot.state_id),
        );
        let context_blob = ContextBlob::new(vec![annotation.clone()]);
        let context_root = repo
            .set_context_blob(None, &target, &context_blob)
            .expect("store context");
        repo.put_state_attachment(&StateAttachment {
            state_id: snapshot.state_id,
            body: StateAttachmentBody::Context(context_root),
            attribution: Attribution::human(principal),
            created_at: Utc::now(),
            supersedes: None,
        })
        .expect("attach context");

        let resolved = PullBootstrapMetadata {
            discussions_from_pack: true,
            discussions: Vec::new(),
            context_from_pack: true,
            context: Vec::new(),
        }
        .resolve(&repo, Some(snapshot.state_id))
        .expect("resolve packed bootstrap");

        assert_eq!(resolved.discussions, Some(vec![discussion]));
        assert!(resolved.discussions_pack_fallback.is_none());
        assert_eq!(resolved.context, Some(vec![(target, context_blob)]));
        assert!(resolved.context_pack_fallback.is_none());
    }

    fn seed_snapshot() -> (TempDir, Repository, StateId) {
        let temp = TempDir::new().expect("temp repo");
        let repo = Repository::init_default(temp.path()).expect("init repo");
        std::fs::write(temp.path().join("lib.rs"), "pub fn run() {}\n").expect("write source");
        let snapshot = repo
            .snapshot(Some("seed".to_string()), None)
            .expect("snapshot");
        (temp, repo, snapshot.state_id)
    }

    fn sample_discussion(state_id: StateId) -> Discussion {
        Discussion {
            id: "discussion-1".to_string(),
            anchor: SymbolAnchor::new("lib.rs", "run"),
            opened_against_state: state_id,
            opened_at: 1_700_000_000,
            thread_ref: None,
            turns: vec![DiscussionTurn {
                author: Principal::new("Ada", "ada@example.com"),
                body: "keep this invariant".to_string(),
                posted_at: 1_700_000_001,
                references: Vec::new(),
            }],
            resolution: DiscussionResolution::Open,
            body_changed_since_open: false,
            anchor_ambiguous: false,
            orphaned: false,
            visibility: VisibilityTier::Public,
            resolved_annotation_id: None,
        }
    }

    fn packed_metadata() -> PullBootstrapMetadata {
        PullBootstrapMetadata {
            discussions_from_pack: true,
            discussions: Vec::new(),
            context_from_pack: false,
            context: Vec::new(),
        }
    }

    fn packed_context_metadata() -> PullBootstrapMetadata {
        PullBootstrapMetadata {
            discussions_from_pack: false,
            discussions: Vec::new(),
            context_from_pack: true,
            context: Vec::new(),
        }
    }

    #[test]
    fn packed_bootstrap_falls_back_when_context_attachment_is_missing() {
        let (_temp, repo, state_id) = seed_snapshot();
        let resolved = packed_context_metadata()
            .resolve(&repo, Some(state_id))
            .expect("missing packed context must not clone-kill");
        assert!(
            resolved.context.is_none(),
            "None means ObserveCollaboration, not an empty inline set"
        );
        let warning = resolved.context_pack_fallback.expect("warned fallback");
        assert!(warning.contains("the attachment is missing"), "{warning}");
        assert!(
            warning.contains("falling back to ObserveCollaboration"),
            "{warning}"
        );
    }

    #[test]
    fn packed_bootstrap_fail_closes_when_context_blob_is_corrupt() {
        let (_temp, repo, state_id) = seed_snapshot();
        let corrupt_hash = repo
            .store()
            .put_blob(&Blob::from(vec![0x80]))
            .expect("store corrupt context blob");
        let mut files = Tree::new();
        files.insert(TreeEntry::file("lib.rs", corrupt_hash, false).expect("context file entry"));
        let files_hash = repo.store().put_tree(&files).expect("store files tree");
        let mut root = Tree::new();
        root.insert(TreeEntry::directory("__files", files_hash).expect("context files root"));
        let context_root = repo.store().put_tree(&root).expect("store context root");
        repo.put_state_attachment(&StateAttachment {
            state_id,
            body: StateAttachmentBody::Context(context_root),
            attribution: Attribution::human(Principal::new("Ada", "ada@example.com")),
            created_at: Utc::now(),
            supersedes: None,
        })
        .expect("attach corrupt context");

        let error = packed_context_metadata()
            .resolve(&repo, Some(state_id))
            .expect_err("present corrupt ContextBlob must fail closed");
        assert!(
            error.to_string().contains("invalid context blob"),
            "{error}"
        );
    }

    #[test]
    fn packed_bootstrap_falls_back_when_discussions_attachment_is_missing() {
        let (_temp, repo, state_id) = seed_snapshot();
        let resolved = packed_metadata()
            .resolve(&repo, Some(state_id))
            .expect("missing packed discussions must not clone-kill");
        assert!(
            resolved.discussions.is_none(),
            "None means ObserveCollaboration, not an empty inline set"
        );
        let warning = resolved.discussions_pack_fallback.expect("warned fallback");
        assert!(warning.contains("the attachment is missing"), "{warning}");
        assert!(
            warning.contains("falling back to ObserveCollaboration"),
            "{warning}"
        );
        assert!(
            !warning.contains(
                "pull bootstrap advertised packed discussions but the attachment is missing"
            ),
            "the clone-killer error string must not return as success warning verbatim: {warning}"
        );
    }

    #[test]
    fn packed_bootstrap_falls_back_on_version_skew() {
        let (_temp, repo, state_id) = seed_snapshot();
        let principal = Principal::new("Ada", "ada@example.com");
        #[derive(serde::Serialize)]
        struct NamedV2 {
            format_version: u8,
            discussions: Vec<Discussion>,
        }
        let bytes = rmp_serde::to_vec_named(&NamedV2 {
            format_version: 2,
            discussions: vec![sample_discussion(state_id)],
        })
        .expect("encode weft-like named-map v2");
        let hash = repo
            .store()
            .put_blob(&Blob::from(bytes))
            .expect("store v2 blob");
        repo.put_state_attachment(&StateAttachment {
            state_id,
            body: StateAttachmentBody::Discussions(hash),
            attribution: Attribution::human(principal),
            created_at: Utc::now(),
            supersedes: None,
        })
        .expect("attach v2 discussions");

        let resolved = packed_metadata()
            .resolve(&repo, Some(state_id))
            .expect("version skew must not clone-kill");
        assert!(resolved.discussions.is_none());
        let warning = resolved.discussions_pack_fallback.expect("warned fallback");
        assert!(
            warning.contains("falling back to ObserveCollaboration"),
            "{warning}"
        );
        assert!(
            warning.contains("unsupported blob version")
                || warning.contains("blob encoding is not this client's DiscussionsBlob v1"),
            "{warning}"
        );
    }

    #[test]
    fn packed_bootstrap_falls_back_on_foreign_named_map_version() {
        let (_temp, repo, state_id) = seed_snapshot();
        #[derive(serde::Serialize)]
        struct ForeignRoot {
            version: u8,
            kind: String,
        }
        let bytes = rmp_serde::to_vec_named(&ForeignRoot {
            version: 2,
            kind: "DiscussionRootFrame".to_string(),
        })
        .expect("encode foreign named map");
        let hash = repo
            .store()
            .put_blob(&Blob::from(bytes))
            .expect("store foreign root");
        repo.put_state_attachment(&StateAttachment {
            state_id,
            body: StateAttachmentBody::Discussions(hash),
            attribution: Attribution::human(Principal::new("Ada", "ada@example.com")),
            created_at: Utc::now(),
            supersedes: None,
        })
        .expect("attach foreign root");

        let resolved = packed_metadata()
            .resolve(&repo, Some(state_id))
            .expect("positively identified v2 named map must not clone-kill");
        assert!(resolved.discussions.is_none());
        let warning = resolved.discussions_pack_fallback.expect("warned fallback");
        assert!(
            warning.contains("blob encoding is not this client's DiscussionsBlob v1"),
            "{warning}"
        );
    }

    #[test]
    fn packed_bootstrap_fail_closes_when_discussions_blob_is_corrupt() {
        let (_temp, repo, state_id) = seed_snapshot();
        let mut discussion = sample_discussion(state_id);
        discussion.id.clear();
        let blob = DiscussionsBlob {
            format_version: DiscussionsBlob::FORMAT_VERSION,
            discussions: vec![discussion],
        };
        let bytes = rmp_serde::to_vec(&blob).expect("encode corrupt v1 without validate");
        let hash = repo
            .store()
            .put_blob(&Blob::from(bytes))
            .expect("store corrupt blob");
        repo.put_state_attachment(&StateAttachment {
            state_id,
            body: StateAttachmentBody::Discussions(hash),
            attribution: Attribution::human(Principal::new("Ada", "ada@example.com")),
            created_at: Utc::now(),
            supersedes: None,
        })
        .expect("attach corrupt discussions");

        let error = packed_metadata()
            .resolve(&repo, Some(state_id))
            .expect_err("decoded v1 with invalid items is true corruption");
        assert!(
            error
                .to_string()
                .contains("discussion id must not be empty"),
            "{error}"
        );
    }

    #[test]
    fn packed_bootstrap_fail_closes_when_discussions_blob_is_missing() {
        let (_temp, repo, state_id) = seed_snapshot();
        let missing = ContentHash::compute(b"not-stored-discussions");
        repo.put_state_attachment(&StateAttachment {
            state_id,
            body: StateAttachmentBody::Discussions(missing),
            attribution: Attribution::human(Principal::new("Ada", "ada@example.com")),
            created_at: Utc::now(),
            supersedes: None,
        })
        .expect("attach dangling discussions hash");

        let error = packed_metadata()
            .resolve(&repo, Some(state_id))
            .expect_err("attachment present with missing blob is corruption");
        assert!(
            error
                .to_string()
                .contains("packed discussions attachment references missing blob"),
            "{error}"
        );
    }

    #[test]
    fn packed_bootstrap_fail_closes_when_discussions_blob_is_truncated_v1() {
        let (_temp, repo, state_id) = seed_snapshot();
        let valid = DiscussionsBlob::new(vec![sample_discussion(state_id)])
            .encode()
            .expect("encode v1");
        let truncated = &valid[..valid.len().saturating_sub(2)];
        let hash = repo
            .store()
            .put_blob(&Blob::from(truncated.to_vec()))
            .expect("store truncated v1");
        repo.put_state_attachment(&StateAttachment {
            state_id,
            body: StateAttachmentBody::Discussions(hash),
            attribution: Attribution::human(Principal::new("Ada", "ada@example.com")),
            created_at: Utc::now(),
            supersedes: None,
        })
        .expect("attach truncated discussions");

        packed_metadata()
            .resolve(&repo, Some(state_id))
            .expect_err("truncated v1 positional blob is corruption, not version skew");
    }

    #[test]
    fn packed_bootstrap_fail_closes_on_empty_named_map() {
        let (_temp, repo, state_id) = seed_snapshot();
        // MessagePack empty fixmap (`0x80`): map-shaped but not a v2 schema.
        let hash = repo
            .store()
            .put_blob(&Blob::from(vec![0x80]))
            .expect("store empty map");
        repo.put_state_attachment(&StateAttachment {
            state_id,
            body: StateAttachmentBody::Discussions(hash),
            attribution: Attribution::human(Principal::new("Ada", "ada@example.com")),
            created_at: Utc::now(),
            supersedes: None,
        })
        .expect("attach empty map");

        packed_metadata()
            .resolve(&repo, Some(state_id))
            .expect_err("empty map is corruption, not version skew");
    }

    #[test]
    fn unpacked_bootstrap_keeps_inline_discussions() {
        let (_temp, repo, state_id) = seed_snapshot();
        let discussion = sample_discussion(state_id);
        let resolved = PullBootstrapMetadata {
            discussions_from_pack: false,
            discussions: vec![discussion.clone()],
            context_from_pack: false,
            context: Vec::new(),
        }
        .resolve(&repo, Some(state_id))
        .expect("inline bootstrap");
        assert_eq!(resolved.discussions, Some(vec![discussion]));
        assert!(resolved.discussions_pack_fallback.is_none());
        assert_eq!(resolved.context, Some(Vec::new()));
        assert!(resolved.context_pack_fallback.is_none());
    }

    #[test]
    fn hosted_ref_from_api_requires_32_byte_state_id() {
        let entry = api::heddle::api::v1alpha1::RefEntry {
            name: "main".to_string(),
            state_id: Some(api::heddle::api::v1alpha1::StateId { value: vec![0; 8] }),
            is_thread: true,
            revision_address: String::new(),
            thread_id: String::new(),
        };
        assert!(hosted_ref_from_api(&entry).is_err());
        let ok = api::heddle::api::v1alpha1::RefEntry {
            name: "main".to_string(),
            state_id: Some(api::heddle::api::v1alpha1::StateId { value: vec![0; 32] }),
            is_thread: true,
            revision_address: String::new(),
            thread_id: String::new(),
        };
        assert_eq!(
            hosted_ref_from_api(&ok).expect("32-byte state").state_id,
            StateId::from_bytes([0; 32])
        );
    }
}

#[cfg(test)]
mod native_exchange_tests {
    use objects::object::{Attribution, Principal};
    use repo::{
        ThreadFreshness, ThreadManager, ThreadMode, ThreadRecord, ThreadState,
        ThreadVerificationSummary,
    };
    use tempfile::TempDir;
    use wire::ProtocolError;

    use super::{super::PullMaterialization, *};

    const CLONE_BOOTSTRAP_THREAD: &str = "heddle-clone-bootstrap-v1:";

    fn repository(temp: &TempDir) -> (Repository, StateId) {
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("tracked.txt"), "content\n").unwrap();
        let state = repo
            .snapshot_with_attribution(
                Some("native exchange fixture".to_string()),
                None,
                Attribution::human(Principal::new("Test", "test@example.com")),
            )
            .unwrap()
            .id();
        (repo, state)
    }

    fn save_thread_record(
        repo: &Repository,
        state: StateId,
        stable_id: &str,
        thread: &str,
    ) -> ThreadRecord {
        let stored_state = repo.store().get_state(&state).unwrap().unwrap();
        let now = chrono::Utc::now();
        let record = ThreadRecord {
            id: stable_id.to_string(),
            thread: thread.to_string(),
            target_thread: None,
            parent_thread: None,
            mode: ThreadMode::Solid,
            state: ThreadState::Active,
            base_state: state.to_string_full(),
            base_root: stored_state.tree.to_hex(),
            current_state: Some(state.to_string_full()),
            merged_state: None,
            task: Some("prove first-push identity".to_string()),
            changed_paths: vec!["tracked.txt".to_string()],
            impact_categories: Vec::new(),
            heavy_impact_paths: Vec::new(),
            promotion_suggested: false,
            freshness: ThreadFreshness::Current,
            verification_summary: ThreadVerificationSummary::default(),
            confidence_summary: Default::default(),
            integration_policy_result: Default::default(),
            created_at: now,
            updated_at: now,
            ephemeral: None,
            auto: false,
            shared_target_dir: None,
        };
        ThreadManager::new(repo.heddle_dir())
            .save_record(&record)
            .unwrap();
        record
    }

    #[test]
    fn persist_advertised_identity_adopts_api_thread_id() {
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let hosted_id = "019f0000-aaaa-7bbb-8ccc-ddddeeeeffff";
        let minted = ThreadManager::new(repo.heddle_dir())
            .find_record_by_thread("main")
            .unwrap()
            .expect("init persists main");
        assert_ne!(minted.id, hosted_id);

        let advertised = [HostedRefEntry::from_advertised(
            "main".to_string(),
            state,
            true,
            format!("heddle:{}", state.to_string_full()),
            Some(hosted_id.to_string()),
        )];
        assert_eq!(advertised[0].thread_id.as_deref(), Some(hosted_id));

        persist_advertised_thread_identity_with_live_fallback(
            &repo,
            &advertised,
            None,
            "main",
            &state,
        )
        .expect("advertised RefEntry thread_id must be adopted without a second ListRefs");

        let persisted = ThreadManager::new(repo.heddle_dir())
            .find_record_by_thread("main")
            .unwrap()
            .expect("clone must persist a main record");
        assert_eq!(persisted.id, hosted_id);
        assert_ne!(persisted.id, minted.id);
    }

    #[test]
    fn persist_advertised_identity_uses_live_refs_when_folded_omit_thread_id() {
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let hosted_id = "019f0000-aaaa-7bbb-8ccc-ddddeeeeffff";
        let minted = ThreadManager::new(repo.heddle_dir())
            .find_record_by_thread("main")
            .unwrap()
            .expect("init persists main");
        assert_ne!(minted.id, hosted_id);

        let folded = [HostedRefEntry::from_advertised(
            "main".to_string(),
            state,
            true,
            format!("heddle:{}", state.to_string_full()),
            None,
        )];
        let live = [HostedRefEntry::from_advertised(
            "main".to_string(),
            state,
            true,
            format!("heddle:{}", state.to_string_full()),
            Some(hosted_id.to_string()),
        )];

        persist_advertised_thread_identity_with_live_fallback(
            &repo,
            &folded,
            Some(&live),
            "main",
            &state,
        )
        .expect("live ListRefs must supply the omitted folded thread_id");

        let persisted = ThreadManager::new(repo.heddle_dir())
            .find_record_by_thread("main")
            .unwrap()
            .expect("clone must persist a main record");
        assert_eq!(persisted.id, hosted_id);
        assert_ne!(persisted.id, minted.id);
    }

    #[tokio::test]
    async fn native_push_without_local_stable_identity_fails_before_sending_request() {
        let (mut client, server, captured) =
            crate::hosted_runtime::hosted::test_server::start_recording_push().await;
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);

        let error = client
            .push_with_expected_head_profiled(
                &repo,
                "acme/widgets",
                state,
                "missing",
                false,
                ExpectedRemoteHead::Missing,
                "missing-local-metadata-test-op".to_string(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid state: native push target 'missing' has no local thread record with a stable identity"
        );
        assert!(
            captured
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .is_empty(),
            "native push must not send an identity-less request"
        );

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_push_and_clone_pull_complete_the_real_framed_exchange() {
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        save_thread_record(&repo, state, "thread-stable-main", "main");

        assert!(client.list_refs("acme/widgets").await.unwrap().is_empty());
        let pushed = client
            .push_with_expected_head_profiled(
                &repo,
                "acme/widgets",
                state,
                "main",
                false,
                ExpectedRemoteHead::Missing,
                "push-test-op".to_string(),
            )
            .await
            .unwrap()
            .0;
        assert!(!pushed.success);
        assert_eq!(pushed.error.as_deref(), Some("test rejection"));

        let clone = TempDir::new().unwrap();
        let (pulled, cloned_repo) = client
            .clone_pull_with_depth_and_materialization(
                "acme/widgets",
                Some("main"),
                Some(1),
                PullMaterialization::Lazy,
                |_, _| Repository::init_default(clone.path()).map_err(ProtocolError::from),
            )
            .await
            .unwrap();
        assert!(!pulled.success);
        assert_eq!(pulled.error.as_deref(), Some("test rejection"));
        assert_eq!(cloned_repo.root(), clone.path());

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn compact_pull_of_a_complete_local_state_publishes_no_synthetic_objects() {
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let remote_state = api::heddle::api::v1alpha1::StateId {
            value: state.as_bytes().to_vec(),
        };
        let (mut client, server) =
            crate::hosted_runtime::hosted::test_server::start_with_remote_state(remote_state).await;

        let received = client
            .fetch_state(&repo, "acme/widgets", CLONE_BOOTSTRAP_THREAD, state)
            .await
            .unwrap();
        assert_eq!(received, 0);

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn complete_local_state_exercises_each_public_pull_mode_without_refetching() {
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let remote_state = api::heddle::api::v1alpha1::StateId {
            value: state.as_bytes().to_vec(),
        };
        let (mut client, server) =
            crate::hosted_runtime::hosted::test_server::start_with_remote_state(remote_state).await;
        let bootstrap = CLONE_BOOTSTRAP_THREAD;

        assert!(
            client
                .pull_profiled(&repo, "acme/widgets", bootstrap, None)
                .await
                .unwrap()
                .0
                .success
        );
        let (profiled, profile) = client
            .pull_profiled(&repo, "acme/widgets", bootstrap, None)
            .await
            .unwrap();
        assert!(profiled.success);
        assert_eq!(profile.objects_received, 0);
        assert!(
            client
                .pull_with_depth(&repo, "acme/widgets", bootstrap, None, Some(1))
                .await
                .unwrap()
                .success
        );
        assert!(
            client
                .pull_with_depth_and_materialization(
                    &repo,
                    "acme/widgets",
                    bootstrap,
                    None,
                    Some(1),
                    PullMaterialization::Lazy,
                )
                .await
                .unwrap()
                .success
        );
        assert!(
            client
                .repair_clone_with_depth_and_materialization(
                    &repo,
                    "acme/widgets",
                    bootstrap,
                    None,
                    PullMaterialization::Full,
                )
                .await
                .unwrap()
                .success
        );
        assert_eq!(
            client
                .hydrate_missing_blobs_for_state(&repo, "acme/widgets", bootstrap, state)
                .await
                .unwrap(),
            0
        );

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn clone_pull_installs_a_complete_native_pack() {
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let objects = wire::enumerate_state_closure(repo.store(), state).unwrap();
        let pack = wire::build_native_pack(repo.store(), &objects).unwrap();
        let remote_state = api::heddle::api::v1alpha1::StateId {
            value: state.as_bytes().to_vec(),
        };
        let (mut client, server) =
            crate::hosted_runtime::hosted::test_server::start_with_pull_pack(
                remote_state,
                pack.pack_data,
                pack.index_data,
            )
            .await;
        let clone = TempDir::new().unwrap();

        let (pulled, cloned_repo) = client
            .clone_pull_with_depth_and_materialization(
                "acme/widgets",
                Some("main"),
                None,
                PullMaterialization::Full,
                |_, _| Repository::init_default(clone.path()).map_err(ProtocolError::from),
            )
            .await
            .unwrap();
        assert!(pulled.success);
        assert_eq!(pulled.final_state, Some(state));
        assert!(cloned_repo.store().has_state(&state).unwrap());
        assert!(
            clone
                .path()
                .join(".heddle/owner-authorization.bin")
                .is_file(),
            "clone must persist the verified PullReady owner genesis before installing objects"
        );

        client.close().await;
        server.await.unwrap();
    }
}
