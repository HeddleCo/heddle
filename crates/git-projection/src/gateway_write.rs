// SPDX-License-Identifier: Apache-2.0
//! Bounded unsigned Git-push preparation and explicit local native acceptance.
//!
//! Preparation takes an explicit caller-selected SourceAuthor and publisher,
//! without a signer or credential discovery. External signatures bind only after
//! fresh sender authority/disclosure checks; this is not hosted Git acceptance.
//! A reader token and a Git author line never grant write authority. Untrusted
//! pack decoding belongs in the host's resource-limited quarantine. Only selected
//! reachable content is staged; no Git refs/caches are copied.
//!
//! The separate local-fixture acceptance adapter requires a pre-existing local
//! owner signer. Native originals, source heads, possession and its retry receipt
//! commit in one native transaction. Native acceptance is not publication of a
//! new Artifacts catalog. Hosts must finish/retry publication before Git ACK.

use crate::{
    GitProjectionError, GitProjectionResult, SyncMapping,
    gateway_view::{
        HistoryTip, ViewLimits, export_public_git_history,
        preflight_prepared_git_history_with_authority,
    },
    git_reconstruct::{commit_object_id, reconstruct_commit_bytes},
};
use crypto::{
    Ed25519Signer, Signer,
    thread_operation::{SignedGenesis, SignedOperation},
};
use objects::{
    object::{
        AudienceTier, Blob, ContentHash, OperationId, State, StateId, ThreadName, Tree, TreeEntry,
        parse_git_tree, reserved_tree_entry_name,
        thread_replication::{
            AuthoredCapture, GenesisOwner, SourceAuthor, ThreadOperation, ThreadOperationBody,
            git_import_converter::{GitImportGraph, GitImportRawCommit},
            git_import_graph::{GitObjectFormat, GitObjectId},
        },
        visible,
    },
    store::ObjectStore,
};
use repo::{
    Repository,
    thread_replication::{
        self,
        source_publication::{Command, PreparedPublication},
    },
};
use serde::{Deserialize, Serialize};
use sley::{GitObjectType, ObjectFormat, ObjectId, Repository as GitRepository};
use std::collections::{BTreeSet, HashMap, HashSet};

fn fail(message: impl std::fmt::Display) -> GitProjectionError {
    GitProjectionError::Git(message.to_string())
}

/// Compressed input bounds complement the host's CPU/address-space/output bounds.
#[derive(Clone, Copy, Debug)]
pub struct WriteLimits {
    pub pack_bytes: usize,
    pub objects: usize,
    pub commits: usize,
    pub entries: usize,
    pub decoded_bytes: usize,
    pub object_bytes: usize,
}
impl Default for WriteLimits {
    fn default() -> Self {
        Self {
            pack_bytes: 16 * 1024 * 1024,
            objects: 20_000,
            commits: 128,
            entries: 10_000,
            decoded_bytes: 64 * 1024 * 1024,
            object_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushUpdate {
    pub thread: String,
    pub old: ObjectId,
    pub new: ObjectId,
}
pub struct ReceivePack<'a> {
    pub update: PushUpdate,
    pub pack: &'a [u8],
}

fn oid(value: &str) -> GitProjectionResult<ObjectId> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || value.bytes().all(|b| b == b'0')
    {
        return Err(fail("nonzero canonical SHA-1 required"));
    }
    value.parse().map_err(fail)
}
fn valid_thread(thread: &str) -> GitProjectionResult<()> {
    if thread.len() > 1024 {
        return Err(fail("branch name limit"));
    }
    ThreadName::from_git_branch(thread).map_err(fail)?;
    Ok(())
}
/// One existing branch, one packet, one flush, then a pack. No deletes, branch
/// creation, arbitrary namespaces, push-options, shallow updates or signed pushes.
/// Advertise only `report-status ofs-delta object-format=sha1` for this contract.
pub fn parse_receive_pack(
    body: &[u8],
    limits: WriteLimits,
) -> GitProjectionResult<ReceivePack<'_>> {
    if body.len() > limits.pack_bytes.saturating_add(4096) || body.len() < 8 {
        return Err(fail("receive-pack body limit"));
    }
    let header = std::str::from_utf8(&body[..4]).map_err(fail)?;
    if !header
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(fail("invalid pkt-line"));
    }
    let len = usize::from_str_radix(header, 16).map_err(fail)?;
    if !(5..=4096).contains(&len) || len + 4 > body.len() || &body[len..len + 4] != b"0000" {
        return Err(fail("exactly one update required"));
    }
    let command = std::str::from_utf8(&body[4..len]).map_err(fail)?;
    let (update, caps) = command
        .split_once('\0')
        .ok_or_else(|| fail("receive capabilities required"))?;
    // Git send-pack writes one optional space after the NUL separator. Other
    // whitespace, repeated spaces and empty capability tokens remain invalid.
    let caps = caps.strip_prefix(' ').unwrap_or(caps);
    let mut seen = HashSet::new();
    for cap in caps.split(' ') {
        if cap.is_empty()
            || !seen.insert(cap)
            || !(matches!(cap, "report-status" | "ofs-delta" | "object-format=sha1")
                || (cap.starts_with("agent=")
                    && cap.len() <= 256
                    && cap.bytes().all(|b| (b'!'..=b'~').contains(&b))))
        {
            return Err(fail("unsupported receive capability"));
        }
    }
    if !seen.contains("report-status") {
        return Err(fail("report-status required"));
    }
    let fields: Vec<_> = update.split(' ').collect();
    if fields.len() != 3 {
        return Err(fail("invalid receive command"));
    }
    let old = oid(fields[0])?;
    let new = oid(fields[1])?;
    if old == new {
        return Err(fail("empty update"));
    }
    let thread = fields[2]
        .strip_prefix("refs/heads/")
        .ok_or_else(|| fail("branch update required"))?;
    valid_thread(thread)?;
    let pack = &body[len + 4..];
    if pack.len() < 32
        || pack.len() > limits.pack_bytes
        || &pack[..4] != b"PACK"
        || pack[4..8] != [0, 0, 0, 2]
    {
        return Err(fail("bounded pack v2 required"));
    }
    let count = u32::from_be_bytes(pack[8..12].try_into().map_err(fail)?) as usize;
    if count > limits.objects {
        return Err(fail("pack object limit"));
    }
    Ok(ReceivePack {
        update: PushUpdate {
            thread: thread.into(),
            old,
            new,
        },
        pack,
    })
}

/// Independent, trusted local-fixture actor. Never derive this from Git metadata.
pub struct LocalWriter<'a> {
    pub actor: &'a str,
    pub signer: &'a Ed25519Signer,
}
pub struct LocalPush<'a> {
    pub update: &'a PushUpdate,
    pub expected_native: StateId,
    /// Current writer/disclosure decision generation, included in retry identity.
    pub policy_generation: &'a str,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushReceipt {
    pub thread: String,
    pub native_state: StateId,
    pub old_git: String,
    pub new_git: String,
    pub actor: String,
    pub command_id: OperationId,
    pub operations: Vec<ContentHash>,
}
pub struct NativeAcceptance {
    pub receipt: PushReceipt,
    pub replayed: bool,
}

struct Decoder<'a> {
    source: &'a GitRepository,
    native: &'a Repository,
    remaining: WriteLimits,
    read: HashSet<ObjectId>,
    blobs: HashMap<ObjectId, ContentHash>,
}
impl Decoder<'_> {
    fn object(&mut self, id: ObjectId, kind: GitObjectType) -> GitProjectionResult<Vec<u8>> {
        let (actual, size) = self
            .source
            .read_object_header(&id)
            .map_err(fail)?
            .ok_or_else(|| fail("missing quarantine object"))?;
        let object_limit = if kind == GitObjectType::Commit {
            self.remaining.object_bytes.min(64 * 1024)
        } else {
            self.remaining.object_bytes
        };
        if actual != kind || size > object_limit as u64 {
            return Err(fail("object type or size limit"));
        }
        if self.read.insert(id) {
            self.remaining.objects = self
                .remaining
                .objects
                .checked_sub(1)
                .ok_or_else(|| fail("object limit"))?;
            let size = usize::try_from(size).map_err(fail)?;
            self.remaining.decoded_bytes = self
                .remaining
                .decoded_bytes
                .checked_sub(size)
                .ok_or_else(|| fail("decoded byte limit"))?;
        }
        let object = self.source.read_object(&id).map_err(fail)?;
        if object.object_type != kind
            || object.body.len() as u64 != size
            || sley_core::object_id_for_bytes(ObjectFormat::Sha1, kind.as_str(), &object.body)
                .map_err(fail)?
                != id
        {
            return Err(fail("quarantine object identity mismatch"));
        }
        Ok(object.body.clone())
    }
    fn blob(&mut self, id: ObjectId) -> GitProjectionResult<ContentHash> {
        if let Some(hash) = self.blobs.get(&id) {
            return Ok(*hash);
        }
        let blob = Blob::new(self.object(id, GitObjectType::Blob)?);
        let hash = blob.hash();
        if self
            .native
            .redaction_stub_for_blob(&hash)
            .map_err(fail)?
            .is_some()
        {
            return Err(fail("redacted blob cannot be republished"));
        }
        self.native.store().put_blob(&blob)?;
        self.blobs.insert(id, hash);
        Ok(hash)
    }
    fn tree(&mut self, id: ObjectId, depth: usize) -> GitProjectionResult<ContentHash> {
        if depth > 64 {
            return Err(fail("tree depth limit"));
        }
        // Do not skip traversal on cache hits: every path occurrence consumes the
        // expanded-tree budget, preventing small DAGs from expanding exponentially.
        let body = self.object(id, GitObjectType::Tree)?;
        // Bound entry allocation BEFORE the shared parser builds its Vec.
        let mut rest = body.as_slice();
        while !rest.is_empty() {
            self.remaining.entries = self
                .remaining
                .entries
                .checked_sub(1)
                .ok_or_else(|| fail("tree entry limit"))?;
            let space = rest
                .iter()
                .take(7)
                .position(|b| *b == b' ')
                .ok_or_else(|| fail("invalid bounded Git mode"))?;
            rest = &rest[space + 1..];
            let nul = rest
                .iter()
                .take(256)
                .position(|b| *b == 0)
                .ok_or_else(|| fail("Git path component limit"))?;
            rest = rest
                .get(nul + 21..)
                .ok_or_else(|| fail("truncated tree object ID"))?;
        }
        let entries = parse_git_tree(ObjectFormat::Sha1, &body).map_err(fail)?;
        let mut native = Vec::with_capacity(entries.len());
        for entry in entries {
            let name = std::str::from_utf8(entry.name).map_err(fail)?;
            let mut digits = Vec::new();
            entry.mode.write_digits(&mut digits);
            if reserved_tree_entry_name(entry.name, true, digits == b"120000").is_some()
                || name.len() > 255
            {
                return Err(fail("reserved metadata path or path limit"));
            }
            let value = match digits.as_slice() {
                b"40000" => TreeEntry::directory(name, self.tree(entry.oid, depth + 1)?),
                b"100644" => TreeEntry::file(name, self.blob(entry.oid)?, false),
                b"100755" => TreeEntry::file(name, self.blob(entry.oid)?, true),
                b"120000" => TreeEntry::symlink(name, self.blob(entry.oid)?),
                _ => return Err(fail("unsupported Git tree mode")),
            }
            .map_err(fail)?;
            native.push(value);
        }
        let tree = Tree::from_git_entries(native).map_err(fail)?;
        let hash = tree.hash();
        self.native.store().put_tree(&tree)?;
        Ok(hash)
    }
}

/// Caller-selected source identity. Git metadata never supplies these fields.
/// Authority must be checked by the caller's verifier; no credentials are discovered.
pub struct PushAuthor<'a> {
    pub actor: &'a str,
    pub publisher: [u8; 32],
    pub source_author: &'a SourceAuthor,
}

/// Immutable command binding. This is observed input, never a grant of authority.
#[derive(Clone, Debug)]
pub struct PushScope {
    pub spool: String,
    pub thread_id: ContentHash,
    pub thread: String,
    pub expected_native: StateId,
    pub old_git: String,
    pub new_git: String,
    pub expected_generation: i64,
    pub actor: String,
    pub publisher: [u8; 32],
    pub policy_generation: String,
    pub source_author: SourceAuthor,
}

/// Exact unsigned source plan. Only content-addressed object staging has happened.
/// Preparation grants no authority, signs nothing and commits no native operation,
/// source head, ref, receipt or remote publication. Final combined-view admission
/// preflight runs in `bind_signed`, after external signing, before any acceptance.
/// Private fields prevent substituting canonical States or unsigned operations.
pub struct PreparedGitPush {
    scope: PushScope,
    receipt: PushReceipt,
    operations: Vec<ThreadOperation>,
    states: Vec<State>,
    historical_originals: Vec<SignedOperation>,
    genesis: SignedGenesis,
    mapping: SyncMapping,
}

/// Sender-validated, immutable signed source bytes. This is NOT a receiving-side
/// compare-and-swap receipt, a native acceptance, or permission to send Git ACK.
pub struct SignedGitPush {
    prepared: PreparedGitPush,
    originals: Vec<SignedOperation>,
    source_originals: Vec<SignedOperation>,
}

impl PreparedGitPush {
    pub fn scope(&self) -> &PushScope {
        &self.scope
    }
    /// Proposed semantic command data, not a persisted acceptance receipt.
    /// Only durable native acceptance can turn these values into a receipt.
    pub fn receipt(&self) -> &PushReceipt {
        &self.receipt
    }
    pub fn operations(&self) -> &[ThreadOperation] {
        &self.operations
    }
    pub fn states(&self) -> &[State] {
        &self.states
    }
    pub fn historical_originals(&self) -> &[SignedOperation] {
        &self.historical_originals
    }
    pub fn signed_genesis(&self) -> &SignedGenesis {
        &self.genesis
    }
    /// Exact prepared Git identity; the synthetic native seed has no Git object.
    pub fn git_oid(&self, state: &StateId) -> Option<ObjectId> {
        self.mapping.get_git(state)
    }

    /// Bind externally produced signatures to the exact prepared bytes. Current
    /// sender policy, head/generation, canonical State bytes and full final-view
    /// disclosure/budgets are checked again under the repository mutation lock.
    /// The caller must supply a freshly opened Repository after any external
    /// signing delay: Repository caches configuration, so a stale handle cannot
    /// establish current export or visibility defaults. Deliberate in-memory
    /// configuration overrides must be reauthorized and reapplied by that caller.
    /// Sidecars and the native ledger are re-read under the mutation lock.
    /// The authority verifier must independently verify each original SourceAuthor
    /// and current publisher/Thread authority; signature validity alone is not it.
    pub fn bind_signed(
        self,
        native: &Repository,
        originals: Vec<SignedOperation>,
        authorize: impl Fn(&PushScope) -> GitProjectionResult<()>,
        verify_authority: impl Fn(
            &repo::thread_replication::ThreadReplica,
            &ThreadOperation,
        ) -> GitProjectionResult<()>,
    ) -> GitProjectionResult<SignedGitPush> {
        let _lock = objects::lock::RepoLock::at(native.heddle_dir().join("locks/repo.lock"))
            .write()
            .map_err(fail)?;
        authorize(&self.scope)?;
        let replica = native.native_thread(&self.scope.thread).map_err(fail)?;
        let current = replica.projection().map_err(fail)?;
        let heads = if current.source_heads.is_empty() {
            vec![current.genesis.base]
        } else {
            current.source_heads
        };
        if replica.thread_id() != self.scope.thread_id
            || current.genesis.spool != self.scope.spool
            || current.generation != self.scope.expected_generation
            || heads != [self.scope.expected_native]
            || replica.signed_genesis().map_err(fail)? != self.genesis
        {
            return Err(fail("prepared native scope or head changed"));
        }
        if originals.len() != self.operations.len() {
            return Err(fail("signed operation count differs from preparation"));
        }
        for (original, expected) in originals.iter().zip(&self.operations) {
            if original.canonical != expected.encode()?
                || original.verify().map_err(fail)? != *expected
            {
                return Err(fail("signed operation differs from exact preparation"));
            }
        }
        for state in &self.states {
            let stored = native
                .store()
                .get_state(&state.id())?
                .ok_or_else(|| fail("prepared State missing"))?;
            if stored.encode_current_msgpack()? != state.encode_current_msgpack()? {
                return Err(fail("prepared canonical State changed"));
            }
        }
        // External signing may take time. A StateId does not freeze current
        // captured reference/visibility sidecars, so reproduce the exact capture
        // selection under today's metadata and refuse any silent substitution.
        let mut refreshed_parents = Vec::new();
        for operation in &self.operations {
            let state = operation
                .source_state()?
                .ok_or_else(|| fail("prepared source State missing"))?;
            let refreshed = replica
                .prepare_capture_with_prepared(native, &state, &refreshed_parents)
                .map_err(fail)?;
            if operation.source_result()? != Some(refreshed) {
                return Err(fail("prepared capture metadata changed"));
            }
            refreshed_parents.push(operation.clone());
        }
        preflight_prepared_git_history_with_authority(
            native,
            self.receipt.native_state,
            &self.scope.thread,
            &originals,
            ViewLimits::default(),
            verify_authority,
        )?;
        // The public parent projection also depends on current native export
        // context. Rebuild it rather than trusting a StateId-only observation
        // across an unlocked external-signing interval, then prove every new
        // commit still reconstructs the exact originally requested Git identity.
        let projected = tempfile::tempdir()?;
        let sink = GitRepository::init_bare(projected.path().join("public.git")).map_err(fail)?;
        let mut mapping = export_public_git_history(
            native,
            &sink,
            &[HistoryTip {
                thread: &self.scope.thread,
                state: self.scope.expected_native,
            }],
            &[&self.scope.thread],
            ViewLimits::default(),
        )?;
        for operation in &self.operations {
            let state = operation
                .source_state()?
                .ok_or_else(|| fail("prepared source State missing"))?;
            let bytes = reconstruct_commit_bytes(native, &sink, &mapping, &state)?;
            mapping.insert_checked(state.id(), commit_object_id(&bytes))?;
        }
        if mapping != self.mapping {
            return Err(fail("prepared Git history identities changed"));
        }
        let mut source_originals = self.historical_originals.clone();
        for original in &originals {
            if !source_originals
                .iter()
                .any(|existing| existing.canonical == original.canonical)
            {
                source_originals.push(original.clone());
            }
        }
        Ok(SignedGitPush {
            prepared: self,
            originals,
            source_originals,
        })
    }
}
/// A local preparation result cannot itself prove the receiver-owned native
/// head fence. Hosted acceptance must use the separate native publication RPC.
#[derive(Debug, thiserror::Error)]
#[error(
    "hosted Git acceptance requires an authoritative upstream expected-head compare-and-swap; source-byte publication alone is insufficient"
)]
pub struct HostedHeadFenceUnavailable;

impl SignedGitPush {
    /// Explicit fail-closed boundary. No caller-supplied success token, byte
    /// publication receipt or sender validation can stand in for upstream CAS.
    /// This local adapter makes no hosted request and never sends Git success.
    pub fn require_hosted_git_acceptance(
        &self,
    ) -> std::result::Result<NativeAcceptance, HostedHeadFenceUnavailable> {
        Err(HostedHeadFenceUnavailable)
    }
    pub fn scope(&self) -> &PushScope {
        self.prepared.scope()
    }
    /// Proposed semantic command data, not a persisted acceptance receipt.
    /// Only durable native acceptance can turn these values into a receipt.
    pub fn receipt(&self) -> &PushReceipt {
        self.prepared.receipt()
    }
    pub fn states(&self) -> &[State] {
        self.prepared.states()
    }
    pub fn signed_genesis(&self) -> &SignedGenesis {
        self.prepared.signed_genesis()
    }
    pub fn git_oid(&self, state: &StateId) -> Option<ObjectId> {
        self.prepared.git_oid(state)
    }
    /// Exact newly signed originals, in prepared parent-before-child order.
    pub fn originals(&self) -> &[SignedOperation] {
        &self.originals
    }
    /// Complete retained plus new same-Thread source originals for SourcePack.
    pub fn source_originals(&self) -> &[SignedOperation] {
        &self.source_originals
    }
}

/// Prepare a bounded Git fast-forward without signing or admitting source.
/// `authorize` must verify the explicit author/publisher/actor and policy scope.
/// Quarantine requirements are identical to `accept_local_fixture_push`.
/// The returned plan releases the repository lock; signing may happen elsewhere.
/// `bind_signed` is mandatory before publication and revalidates the observation.
pub fn prepare_git_push(
    native: &Repository,
    quarantine: &GitRepository,
    request: LocalPush<'_>,
    author: PushAuthor<'_>,
    limits: WriteLimits,
    authorize: impl Fn(&PushScope) -> GitProjectionResult<()>,
) -> GitProjectionResult<PreparedGitPush> {
    let _lock = objects::lock::RepoLock::at(native.heddle_dir().join("locks/repo.lock"))
        .write()
        .map_err(fail)?;
    let update = request.update;
    validate_input(quarantine, &request, author.actor, limits)?;
    author.source_author.validate()?;
    let replica = native.native_thread(&update.thread).map_err(fail)?;
    let projection = replica.projection().map_err(fail)?;
    if let SourceAuthor::Account { spool, .. } = author.source_author
        && spool.to_string() != projection.genesis.spool
    {
        return Err(fail("explicit source author Spool differs"));
    }
    let publisher = author.publisher;
    let source_author = author.source_author;
    let actor = author.actor;
    let scope = PushScope {
        spool: projection.genesis.spool.clone(),
        thread_id: replica.thread_id(),
        thread: update.thread.clone(),
        expected_native: request.expected_native,
        old_git: update.old.to_string(),
        new_git: update.new.to_string(),
        expected_generation: projection.generation,
        actor: actor.into(),
        publisher,
        policy_generation: request.policy_generation.into(),
        source_author: source_author.clone(),
    };
    authorize(&scope)?;
    let (command_id, _) = command_identity(&scope)?;
    if !visible(
        &native.resolve_capture_default_visibility(),
        &AudienceTier::Public,
    ) {
        return Err(fail("local capture default is not public"));
    }
    let heads = if projection.source_heads.is_empty() {
        vec![projection.genesis.base]
    } else {
        projection.source_heads
    };
    if heads != [request.expected_native] {
        return Err(fail("expected native head changed or conflicted"));
    }
    let projected = tempfile::tempdir()?;
    let sink = GitRepository::init_bare(projected.path().join("public.git")).map_err(fail)?;
    let mut mapping = export_public_git_history(
        native,
        &sink,
        &[HistoryTip {
            thread: &update.thread,
            state: request.expected_native,
        }],
        &[&update.thread],
        ViewLimits::default(),
    )?;
    if mapping.get_git(&request.expected_native) != Some(update.old) {
        return Err(fail("expected Git head differs from native projection"));
    }
    let mut decoder = Decoder {
        source: quarantine,
        native,
        remaining: limits,
        read: HashSet::new(),
        blobs: HashMap::new(),
    };
    let mut pending = vec![(update.new, false)];
    let mut visited = HashSet::new();
    let mut ordered = Vec::new();
    let mut raw = HashMap::new();
    let mut reaches_old = false;
    while let Some((id, finish)) = pending.pop() {
        if id == update.old {
            reaches_old = true;
        }
        if mapping.get_heddle(id).is_some() {
            continue;
        }
        if finish {
            ordered.push(id);
            continue;
        }
        if !visited.insert(id) {
            continue;
        }
        if visited.len() > limits.commits {
            return Err(fail("commit limit"));
        }
        let body = decoder.object(id, GitObjectType::Commit)?;
        let commit = sley::CommitObject::parse_ref(ObjectFormat::Sha1, &body).map_err(fail)?;
        if commit.parents.is_empty() || commit.parents.len() > 16 {
            return Err(fail("new roots or oversized merges unsupported"));
        }
        // Heddle identity/agent trailers are a native-only feature. Git metadata
        // must never impersonate an existing native Change or source author.
        if commit
            .message
            .split(|b| *b == b'\n')
            .any(|line| line.starts_with(b"Heddle-"))
        {
            return Err(fail(
                "native identity trailers are not writable through Git",
            ));
        }
        pending.push((id, true));
        for parent in commit.parents.iter().rev() {
            pending.push((*parent, false));
        }
        raw.insert(id, body);
    }
    if !reaches_old || ordered.is_empty() {
        return Err(fail("non-fast-forward update"));
    }
    let mut operations = Vec::new();
    let mut operation_for_state = HashMap::new();
    for id in ordered {
        let body = raw
            .get(&id)
            .ok_or_else(|| fail("missing prepared commit"))?;
        let commit = sley::CommitObject::parse_ref(ObjectFormat::Sha1, body).map_err(fail)?;
        let mut parents = Vec::new();
        let mut causal = BTreeSet::new();
        for parent in &commit.parents {
            let state = mapping
                .get_heddle(*parent)
                .ok_or_else(|| fail("unmapped or cyclic commit parent"))?;
            parents.push(state);
            if let Some(operation) = operation_for_state.get(&state) {
                causal.insert(*operation);
            } else if state != projection.genesis.base {
                let admitted = replica
                    .source_operation_page(state, None, 129)
                    .map_err(fail)?;
                if admitted.is_empty() || admitted.len() > 128 {
                    return Err(fail("parent lacks bounded same-Thread admission"));
                }
                causal.extend(admitted);
            }
        }
        let tree = decoder.tree(commit.tree, 0)?;
        let git_oid = GitObjectId::Sha1(
            id.as_bytes()
                .try_into()
                .map_err(|_| fail("SHA-1 required"))?,
        );
        let state = GitImportGraph::convert_raw_commit(
            GitImportRawCommit {
                oid: &git_oid,
                object_format: GitObjectFormat::Sha1,
                raw_commit: body,
                heddle_note: None,
            },
            tree,
            parents,
            false,
            |_| Ok(None),
        )?;
        let reconstructed = reconstruct_commit_bytes(native, &sink, &mapping, &state)?;
        if reconstructed != *body || commit_object_id(&reconstructed) != id {
            return Err(fail("Git commit is not byte-faithfully representable"));
        }
        if let Some(existing) = native.store().get_state(&state.id())?
            && existing.encode_current_msgpack()? != state.encode_current_msgpack()?
        {
            return Err(fail("existing native State differs from Git conversion"));
        }
        native.store().put_state(&state)?;
        let visibility = native
            .content_visibility_for_audience(&state.id(), &AudienceTier::Public)
            .map_err(fail)?
            .ok_or_else(|| fail("new source closure is not public"))?;
        if !visibility.is_empty()
            || !visible(
                &native
                    .effective_visibility_tier(&state.id())
                    .map_err(fail)?,
                &AudienceTier::Public,
            )
        {
            return Err(fail("new source closure requires partial disclosure"));
        }
        let result = replica
            .prepare_capture_with_prepared(native, &state, &operations)
            .map_err(fail)?;
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: causal,
            publisher,
            body: ThreadOperationBody::Capture(AuthoredCapture {
                result,
                author: source_author.clone(),
            }),
        };
        operation_for_state.insert(state.id(), operation.id().map_err(fail)?);
        operations.push(operation);
        mapping.insert(state.id(), id);
    }
    let state = mapping
        .get_heddle(update.new)
        .ok_or_else(|| fail("new head missing"))?;
    let receipt = PushReceipt {
        thread: update.thread.clone(),
        native_state: state,
        old_git: update.old.to_string(),
        new_git: update.new.to_string(),
        actor: actor.into(),
        command_id,
        operations: operations
            .iter()
            .map(|op| op.id().map_err(fail))
            .collect::<GitProjectionResult<_>>()?,
    };

    let mut states = Vec::new();
    let mut pending = vec![state];
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        // The final-view limit is rechecked with exact signed originals in bind_signed.
        if seen.len() > ViewLimits::default().states {
            return Err(fail("state limit"));
        }
        let value = native
            .store()
            .get_state(&id)?
            .ok_or_else(|| fail("prepared history State missing"))?;
        pending.extend(value.parents.iter().copied());
        states.push(value);
    }
    let revisions: Vec<_> = states.iter().map(State::id).collect();
    let historical_originals = replica
        .accepted_source_originals_for_revisions(&revisions)
        .map_err(fail)?
        .into_iter()
        .map(|(_, signed)| signed)
        .collect();
    let genesis = replica.signed_genesis().map_err(fail)?;
    Ok(PreparedGitPush {
        scope,
        receipt,
        operations,
        states,
        historical_originals,
        genesis,
        mapping,
    })
}

fn command_identity(scope: &PushScope) -> GitProjectionResult<(OperationId, ContentHash)> {
    // Keep the frozen local-fixture semantic request identity exactly unchanged.
    let mut payload = serde_json::json!({"version":1,"spool":scope.spool,
        "thread":scope.thread_id,"ref":format!("refs/heads/{}",scope.thread),
        "expected_native":scope.expected_native,"old":scope.old_git,"new":scope.new_git,
        "actor":scope.actor,"publisher":scope.publisher.as_slice(),"policy":scope.policy_generation});
    if !matches!(scope.source_author, SourceAuthor::LocalKey) {
        payload["source_author"] = serde_json::to_value(&scope.source_author).map_err(fail)?;
    }
    let payload = serde_json::to_vec(&payload).map_err(fail)?;
    let digest = ContentHash::compute_typed("heddle-local-git-push-v1", &payload);
    let hex = digest.as_bytes()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Ok((hex.parse().map_err(fail)?, digest))
}

fn validate_input(
    quarantine: &GitRepository,
    request: &LocalPush<'_>,
    actor: &str,
    limits: WriteLimits,
) -> GitProjectionResult<()> {
    valid_thread(&request.update.thread)?;
    if oid(&request.update.old.to_string())? == oid(&request.update.new.to_string())? {
        return Err(fail("empty update"));
    }
    if actor.is_empty()
        || actor.len() > 1024
        || actor.contains('\0')
        || request.policy_generation.is_empty()
        || request.policy_generation.len() > 1024
        || limits.commits == 0
        || limits.commits > 128
    {
        return Err(fail("bounded local writer scope required"));
    }
    if quarantine.object_format() != ObjectFormat::Sha1
        || quarantine.git_dir().join("shallow").exists()
        || quarantine
            .git_dir()
            .join("objects/info/alternates")
            .exists()
        || quarantine
            .git_dir()
            .join("objects/info/http-alternates")
            .exists()
        || quarantine.git_dir().join("refs/replace").exists()
    {
        return Err(fail("isolated complete SHA-1 quarantine required"));
    }
    Ok(())
}

/// Accept only a fast-forward of one already-created local native Thread.
///
/// `quarantine` MUST be a fresh, host-owned, bounded/unshared Git repository with
/// no alternates, shallow boundary, replace refs, hooks or user configuration.
/// The host must resolve incoming thin-pack bases only from its authorized public
/// projection and discard quarantine after this call. This routine never reads
/// refs/reflogs/notes, consults a hidden Git mirror, or imports unreachable objects.
///
/// `authorize` checks the independently authenticated writer and current full
/// disclosure scope. It runs before replay/preparation and INSIDE the final native
/// transaction before commit. The repository mutation lock covers disclosure
/// preflight through commit. An error rolls back source heads and receipt.
/// Immutable object staging may remain unreachable after any failed attempt.
/// No signing identity is created, no hosted claim is accepted, no catalog is
/// published and no compatibility checkout/ref is moved by this function.
pub fn accept_local_fixture_push(
    native: &Repository,
    quarantine: &GitRepository,
    request: LocalPush<'_>,
    writer: LocalWriter<'_>,
    limits: WriteLimits,
    authorize: impl Fn() -> GitProjectionResult<()>,
) -> GitProjectionResult<NativeAcceptance> {
    authorize()?;
    // Public disclosure sidecars and native SQL use this same cross-process,
    // same-thread-reentrant repository lock. Keep preflight and acceptance in
    // one mutation boundary; generation CAS additionally guards native heads.
    let _serialization = objects::lock::RepoLock::at(native.heddle_dir().join("locks/repo.lock"))
        .write()
        .map_err(fail)?;
    let update = request.update;
    validate_input(quarantine, &request, writer.actor, limits)?;
    let replica = native.native_thread(&update.thread).map_err(fail)?;
    let projection = replica.projection().map_err(fail)?;
    let publisher: [u8; 32] = writer
        .signer
        .public_key()
        .try_into()
        .map_err(|_| fail("publisher key width"))?;
    if projection.genesis.owner != GenesisOwner::LocalKey(publisher)
        || replica.effective_owner().map_err(fail)? != GenesisOwner::LocalKey(publisher)
        || !replica.ownership_claims().map_err(fail)?.is_empty()
    {
        return Err(fail("explicit existing local Thread owner required"));
    }
    let payload=serde_json::to_vec(&serde_json::json!({"version":1,"spool":projection.genesis.spool,
        "thread":replica.thread_id(),"ref":format!("refs/heads/{}",update.thread),
        "expected_native":request.expected_native,"old":update.old.to_string(),"new":update.new.to_string(),
        "actor":writer.actor,"publisher":publisher.as_slice(),"policy":request.policy_generation})).map_err(fail)?;
    let digest = ContentHash::compute_typed("heddle-local-git-push-v1", &payload);
    let hex = digest.as_bytes()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let command_id: OperationId = hex.parse().map_err(fail)?;
    let command = || Command {
        namespace: writer.actor,
        id: command_id,
        method: "LocalGitPush",
        request_hash: *digest.as_bytes(),
    };
    let replay = repo::device_operations::replay_response(
        native.heddle_dir(),
        &repo::device_operations::Command {
            namespace: writer.actor,
            id: command_id,
            method: "LocalGitPush",
            request_hash: *digest.as_bytes(),
        },
    )
    .map_err(fail)?;
    if let Some(bytes) = replay {
        authorize()?;
        return Ok(NativeAcceptance {
            receipt: serde_json::from_slice(&bytes).map_err(fail)?,
            replayed: true,
        });
    }
    let prepared = prepare_git_push(
        native,
        quarantine,
        request,
        PushAuthor {
            actor: writer.actor,
            publisher,
            source_author: &SourceAuthor::LocalKey,
        },
        limits,
        |_| Ok(()),
    )?;
    let originals = prepared
        .operations()
        .iter()
        .map(|operation| SignedOperation::sign(operation, writer.signer).map_err(fail))
        .collect::<GitProjectionResult<Vec<_>>>()?;
    let signed = prepared.bind_signed(
        native,
        originals,
        |_| Ok(()),
        |replica, operation| replica.verify_local_source_owner(operation).map_err(fail),
    )?;
    let receipt = signed.receipt().clone();
    let state = receipt.native_state;
    let originals = signed.originals();
    let response = serde_json::to_vec(&receipt).map_err(fail)?;
    let guards = [(replica.clone(), projection.generation)];
    let bytes = replica
        .publish_prepared_source(
            PreparedPublication {
                operations: originals,
                authority_admissions: &Default::default(),
                revision: state,
                guards: &guards,
            },
            native.store(),
            command(),
            |candidate, original| {
                let op = original.verify()?;
                if candidate.thread_id() != replica.thread_id() || op.publisher != publisher {
                    return Err(thread_replication::Error::Invalid(
                        "local writer scope changed".into(),
                    ));
                }
                Ok(())
            },
            || {
                authorize().map_err(|e| thread_replication::Error::Invalid(e.to_string()))?;
                Ok(response)
            },
        )
        .map_err(fail)?;
    Ok(NativeAcceptance {
        receipt: serde_json::from_slice(&bytes).map_err(fail)?,
        replayed: false,
    })
}
