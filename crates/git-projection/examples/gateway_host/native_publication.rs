// SPDX-License-Identifier: Apache-2.0
//! Durable token-free native staging for recoverable hosted publication. This stores SourcePacks,
//! signed originals and a native receipt, never a Git pack or source Git mirror. A cloud host must
//! place this directory on genuinely persistent storage; ephemeral Container disk is insufficient.
use crate::{
    Result,
    policy::{canonical, digest, hex},
};
use api::v2::client::RpcTransport;
use heddle_git_projection::{gateway_publication::PreparedHistory, gateway_write::SignedGitPush};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use thread_api::{
    Remote,
    contract::*,
    publication::{GitHistoryUpload, PublicationOriginals},
    transport,
};

const MAX_METADATA: u64 = 16 * 1024 * 1024;
const MAX_NATIVE: u64 = 64 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    schema: u32,
    actor: String,
    kind: String,
    scope_bytes: Vec<u8>,
    request_digest: String,
    revisions: Vec<Revision>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    opening_sha256: String,
    originals_sha256: String,
    artifacts: Vec<Artifact>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    sha256: String,
    length: u64,
}
pub struct StagedPublication {
    directory: PathBuf,
    descriptor: Descriptor,
}
fn durable(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("native stage parent absent")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn read(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err("native stage byte limit".into());
    }
    Ok(bytes)
}
fn open(frame: &PublishContentClientFrame) -> Result<&PublishContentOpen> {
    match &frame.body {
        Some(publish_content_client_frame::Body::Open(open)) => Ok(open),
        _ => Err("native stage opening absent".into()),
    }
}
impl StagedPublication {
    /// The caller supplies the already verified actor identity. Session secrets are added only
    /// when transmitting and are never persisted. Every external signer remains caller-selected.
    pub async fn stage(
        root: &Path,
        history: &PreparedHistory,
        push: &SignedGitPush,
        actor: &str,
        actor_scope: &GitTransportScope,
        expected_native_generation: i64,
    ) -> Result<Self> {
        if expected_native_generation < 0
            || !root.is_absolute()
            || history.revisions().is_empty()
            || history.revisions().len() > 128
            || history.tip() != push.receipt().native_state
            || actor != push.scope().actor
        {
            return Err("exact bounded native stage required".into());
        }
        let mut openings = history
            .revisions()
            .iter()
            .map(|r| r.publication().opening().clone())
            .collect::<Vec<_>>();
        let prior = openings
            .iter()
            .take(openings.len() - 1)
            .map(|f| {
                Ok(GitPushHistoryRevision {
                    client_operation_id: f.client_operation_id.clone(),
                    open: Some(open(f)?.clone()),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let tip = openings.last_mut().ok_or("native tip absent")?;
        let Some(publish_content_client_frame::Body::Open(tip_open)) = tip.body.as_mut() else {
            return Err("native tip opening absent".into());
        };
        let old: sley::ObjectId = push.scope().old_git.parse()?;
        let new: sley::ObjectId = push.scope().new_git.parse()?;
        let originals_digest = thread_api::publication::git_originals_digest(
            history
                .revisions()
                .iter()
                .map(|r| r.publication().originals()),
        )?;
        tip_open.protocol = Some(api::heddle::api::common::ProtocolCompatibility {
            protocol_version: 2,
            mandatory_features: vec![1, 2],
        });
        tip_open.git_acceptance = Some(GitPushAcceptance {
            expected_revision: push.scope().expected_native.as_bytes().to_vec(),
            expected_git_commit: old.as_bytes().to_vec(),
            accepted_git_commit: new.as_bytes().to_vec(),
            gateway_publisher: push.scope().publisher.to_vec(),
            transport_token: String::new(),
            history: prior,
            scope: Some(actor_scope.clone()),
            expected_native_generation: Some(expected_native_generation),
            originals_digest: originals_digest.to_vec(),
        });
        let request_digest = api::git_acceptance::request_digest(tip, actor)
            .ok_or("native request identity absent")?;
        let request_digest = request_digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        Self::store(
            root,
            history,
            openings,
            actor,
            actor_scope,
            "git-push",
            request_digest,
        )
        .await
    }
    /// Stage an ALREADY admitted native head for initial Git discovery. This has no Git
    /// acceptance command and can never be submitted as a push or treated as a push ACK.
    pub async fn stage_bootstrap(
        root: &Path,
        history: &PreparedHistory,
        actor: &str,
        actor_scope: &GitTransportScope,
    ) -> Result<Self> {
        let openings = history
            .revisions()
            .iter()
            .map(|r| r.publication().opening().clone())
            .collect::<Vec<_>>();
        if openings
            .iter()
            .any(|f| open(f).is_ok_and(|o| o.git_acceptance.is_some()))
        {
            return Err("bootstrap cannot contain a Git command".into());
        }
        let identity = bootstrap_identity(actor, &actor_scope.encode_to_vec(), &openings)?;
        Self::store(
            root,
            history,
            openings,
            actor,
            actor_scope,
            "native-bootstrap",
            identity,
        )
        .await
    }
    async fn store(
        root: &Path,
        history: &PreparedHistory,
        openings: Vec<PublishContentClientFrame>,
        actor: &str,
        actor_scope: &GitTransportScope,
        kind: &str,
        request_digest: String,
    ) -> Result<Self> {
        if !root.is_absolute()
            || openings.is_empty()
            || openings.len() > 128
            || openings.len() != history.revisions().len()
            || history
                .revisions()
                .last()
                .is_none_or(|r| r.source().revision() != history.tip())
        {
            return Err("bounded native stage required".into());
        }
        fs::create_dir_all(root)?;
        let scratch = tempfile::Builder::new()
            .prefix("native-staging-")
            .tempdir_in(root)?;
        let mut descriptors = Vec::new();
        let mut metadata_total = 0u64;
        let mut native_total = 0u64;
        for (index, (revision, opening)) in history.revisions().iter().zip(&openings).enumerate() {
            let directory = scratch.path().join(index.to_string());
            fs::create_dir(&directory)?;
            let opening_bytes = opening.encode_to_vec();
            let mut original_bytes = Vec::new();
            for body in revision
                .publication()
                .originals()
                .geneses
                .iter()
                .cloned()
                .map(publish_content_client_frame::Body::ThreadGenesis)
                .chain(
                    revision
                        .publication()
                        .originals()
                        .operations
                        .iter()
                        .cloned()
                        .map(publish_content_client_frame::Body::Operations),
                )
            {
                PublishContentClientFrame {
                    client_operation_id: opening.client_operation_id.clone(),
                    body: Some(body),
                }
                .encode_length_delimited(&mut original_bytes)?;
            }
            metadata_total = metadata_total
                .checked_add((opening_bytes.len() + original_bytes.len()) as u64)
                .ok_or("native metadata overflow")?;
            if metadata_total > MAX_METADATA {
                return Err("native metadata stage limit".into());
            }
            durable(&directory.join("opening.pb"), &opening_bytes)?;
            durable(&directory.join("originals.pb"), &original_bytes)?;
            let mut artifacts = Vec::new();
            for (artifact_index, (file, planned)) in revision
                .source()
                .open_artifacts()
                .await?
                .into_iter()
                .zip(revision.source().artifacts())
                .enumerate()
            {
                native_total = native_total
                    .checked_add(planned.length)
                    .ok_or("native source overflow")?;
                if native_total > MAX_NATIVE {
                    return Err("native source stage limit".into());
                }
                let mut bytes = Vec::new();
                file.into_std()
                    .await
                    .take(planned.length + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 != planned.length {
                    return Err("native source length mismatch".into());
                }
                durable(&directory.join(format!("{artifact_index}.native")), &bytes)?;
                artifacts.push(Artifact {
                    sha256: digest(&bytes),
                    length: planned.length,
                });
            }
            File::open(&directory)?.sync_all()?;
            descriptors.push(Revision {
                opening_sha256: digest(&opening_bytes),
                originals_sha256: digest(&original_bytes),
                artifacts,
            });
        }
        let descriptor = Descriptor {
            schema: 1,
            actor: actor.into(),
            kind: kind.into(),
            scope_bytes: actor_scope.encode_to_vec(),
            request_digest: request_digest.clone(),
            revisions: descriptors,
        };
        durable(
            &scratch.path().join("manifest.json"),
            &canonical(&descriptor)?,
        )?;
        File::open(scratch.path())?.sync_all()?;
        let directory = root.join(&request_digest);
        if directory.exists() {
            let prior = Self::load(&directory)?;
            if canonical(&prior.descriptor)? != canonical(&descriptor)? {
                return Err("native stage semantic identity conflict".into());
            }
            return Ok(prior);
        }
        fs::rename(scratch.path(), &directory)?;
        File::open(root)?.sync_all()?;
        Self::load(&directory)
    }
    pub fn load(directory: &Path) -> Result<Self> {
        let bytes = read(&directory.join("manifest.json"), 64 * 1024)?;
        let descriptor: Descriptor = serde_json::from_slice(&bytes)?;
        if canonical(&descriptor)? != bytes
            || descriptor.schema != 1
            || !matches!(descriptor.kind.as_str(), "git-push" | "native-bootstrap")
            || descriptor.scope_bytes.is_empty()
            || descriptor.scope_bytes.len() > 4096
            || !hex(&descriptor.request_digest, 64)
            || descriptor.revisions.is_empty()
            || descriptor.revisions.len() > 128
            || directory.file_name().and_then(|p| p.to_str()) != Some(&descriptor.request_digest)
        {
            return Err("invalid durable native stage".into());
        }
        let staged = Self {
            directory: directory.into(),
            descriptor,
        };
        staged.verify_identity()?;
        Ok(staged)
    }
    /// Semantic native request digest, distinct from the outer operation UUID and Worker journal hash.
    pub fn operation(&self) -> &str {
        &self.descriptor.request_digest
    }
    pub fn client_operation_id(&self) -> Result<String> {
        Ok(self
            .openings()?
            .last()
            .ok_or("native tip absent")?
            .client_operation_id
            .clone())
    }
    pub fn artifact(&self, revision: usize, artifact: usize) -> Result<Vec<u8>> {
        let expected = self
            .descriptor
            .revisions
            .get(revision)
            .and_then(|r| r.artifacts.get(artifact))
            .ok_or("unknown native artifact")?;
        if expected.length > MAX_NATIVE {
            return Err("native artifact limit".into());
        }
        let bytes = read(
            &self
                .directory
                .join(revision.to_string())
                .join(format!("{artifact}.native")),
            expected.length,
        )?;
        if bytes.len() as u64 != expected.length || digest(&bytes) != expected.sha256 {
            return Err("durable native artifact changed".into());
        }
        Ok(bytes)
    }
    pub async fn submit<T: RpcTransport<Error = transport::Error>>(
        &self,
        remote: &Remote<T>,
        session: &str,
    ) -> Result<PublicationReceipt> {
        if self.descriptor.kind != "git-push" {
            return Err("native bootstrap is not a Git acceptance command".into());
        }
        if session.is_empty() {
            return Err("fresh Git session required".into());
        }
        let mut revisions = Vec::new();
        let mut metadata_total = 0u64;
        let mut native_total = 0u64;
        for (index, descriptor) in self.descriptor.revisions.iter().enumerate() {
            let directory = self.directory.join(index.to_string());
            let opening_bytes = read(&directory.join("opening.pb"), 512 * 1024)?;
            let originals_bytes = read(&directory.join("originals.pb"), MAX_METADATA)?;
            metadata_total += (opening_bytes.len() + originals_bytes.len()) as u64;
            if metadata_total > MAX_METADATA
                || digest(&opening_bytes) != descriptor.opening_sha256
                || digest(&originals_bytes) != descriptor.originals_sha256
                || descriptor.artifacts.len() != 2
            {
                return Err("durable native metadata changed".into());
            }
            let opening = PublishContentClientFrame::decode(opening_bytes.as_slice())?;
            let mut originals = PublicationOriginals {
                geneses: Vec::new(),
                operations: Vec::new(),
            };
            let mut rest = originals_bytes.as_slice();
            while !rest.is_empty() {
                let frame = PublishContentClientFrame::decode_length_delimited(&mut rest)?;
                if frame.client_operation_id != opening.client_operation_id {
                    return Err("native original operation mismatch".into());
                }
                match frame.body {
                    Some(publish_content_client_frame::Body::ThreadGenesis(v)) => {
                        originals.geneses.push(v)
                    }
                    Some(publish_content_client_frame::Body::Operations(v)) => {
                        originals.operations.push(v)
                    }
                    _ => return Err("invalid native original frame".into()),
                }
            }
            for (a, expected) in descriptor.artifacts.iter().enumerate() {
                native_total += expected.length;
                self.artifact(index, a)?;
            }
            if native_total > MAX_NATIVE {
                return Err("combined native stage limit".into());
            }
            revisions.push(GitHistoryUpload {
                opening,
                originals,
                artifacts: [
                    tokio::fs::File::open(directory.join("0.native")).await?,
                    tokio::fs::File::open(directory.join("1.native")).await?,
                ],
            });
        }
        let mut tip = revisions.pop().ok_or("native tip absent")?;
        let expected = api::git_acceptance::request_digest(&tip.opening, &self.descriptor.actor)
            .ok_or("native command absent")?;
        if expected
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            != self.descriptor.request_digest
        {
            return Err("durable native intent changed".into());
        }
        let Some(publish_content_client_frame::Body::Open(open)) = tip.opening.body.as_mut() else {
            return Err("native tip missing".into());
        };
        open.git_acceptance
            .as_mut()
            .ok_or("native Git fence absent")?
            .transport_token = session.into();
        // The receiver rechecks the fresh session, native signer and every original on replay.
        let receipt = remote
            .publish_git_content(tip, revisions, &self.descriptor.actor)
            .await?;
        durable(
            &self.directory.join("accepted.pb"),
            &receipt.encode_to_vec(),
        )?;
        Ok(receipt)
    }
}

/// One exact content-addressed native artifact suitable for the private R2 staging adapter.
#[derive(Clone)]
pub struct ExportArtifact {
    pub sha256: String,
    pub bytes: Vec<u8>,
}
impl StagedPublication {
    /// Bounded metadata-only archive. Only allowlisted protobuf plan files and the descriptor
    /// are included; no session token, signer seed, received Git pack or native source body.
    pub fn export_proof(&self) -> Result<Vec<u8>> {
        let mut archive = tar::Builder::new(Vec::new());
        let mut paths = vec!["manifest.json".to_string()];
        for index in 0..self.descriptor.revisions.len() {
            paths.push(format!("{index}/opening.pb"));
            paths.push(format!("{index}/originals.pb"));
        }
        let mut total = 0;
        for path in paths {
            let bytes = read(&self.directory.join(&path), MAX_METADATA)?;
            total += bytes.len() as u64;
            if total > MAX_METADATA + 64 * 1024 {
                return Err("native proof export limit".into());
            }
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_mtime(0);
            header.set_cksum();
            archive.append_data(&mut header, path, bytes.as_slice())?;
        }
        let bytes = archive.into_inner()?;
        if bytes.len() as u64 > MAX_METADATA + 1024 * 1024 {
            return Err("native proof archive limit".into());
        }
        Ok(bytes)
    }
    pub fn export_artifacts(&self) -> Result<Vec<ExportArtifact>> {
        let mut unique = std::collections::BTreeMap::new();
        for (r, revision) in self.descriptor.revisions.iter().enumerate() {
            for (a, artifact) in revision.artifacts.iter().enumerate() {
                if !unique.contains_key(&artifact.sha256) {
                    unique.insert(artifact.sha256.clone(), self.artifact(r, a)?);
                }
            }
        }
        Ok(unique
            .into_iter()
            .map(|(sha256, bytes)| ExportArtifact { sha256, bytes })
            .collect())
    }
    /// Restore exactly the private R2 plan and native objects into disposable local disk. This
    /// does not admit native state or confer authority; submit re-verifies the real receiver.
    pub fn import(root: &Path, proof: &[u8], artifacts: &[ExportArtifact]) -> Result<Self> {
        Self::inspect_proof(proof)?;
        if !root.is_absolute()
            || proof.len() as u64 > MAX_METADATA + 1024 * 1024
            || artifacts.len() > 256
        {
            return Err("bounded native restoration required".into());
        }
        fs::create_dir_all(root)?;
        let scratch = tempfile::tempdir_in(root)?;
        let mut paths = std::collections::BTreeSet::new();
        let mut metadata = 0u64;
        for entry in tar::Archive::new(proof).entries()?.raw(true) {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let text = path.to_str().ok_or("native proof path encoding")?;
            let allowed = text == "manifest.json"
                || text.split_once('/').is_some_and(|(index, file)| {
                    index
                        .parse::<usize>()
                        .is_ok_and(|n| n < 128 && n.to_string() == index)
                        && matches!(file, "opening.pb" | "originals.pb")
                });
            metadata = metadata
                .checked_add(entry.size())
                .ok_or("native proof size overflow")?;
            if !allowed
                || !entry.header().entry_type().is_file()
                || !paths.insert(text.to_string())
                || metadata > MAX_METADATA + 64 * 1024
            {
                return Err("native proof path or metadata limit".into());
            }
            let destination = scratch.path().join(&path);
            fs::create_dir_all(destination.parent().ok_or("native proof parent")?)?;
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            durable(&destination, &bytes)?;
        }
        let descriptor_bytes = read(&scratch.path().join("manifest.json"), 64 * 1024)?;
        let descriptor: Descriptor = serde_json::from_slice(&descriptor_bytes)?;
        if canonical(&descriptor)? != descriptor_bytes
            || descriptor.schema != 1
            || !matches!(descriptor.kind.as_str(), "git-push" | "native-bootstrap")
            || descriptor.scope_bytes.is_empty()
            || descriptor.scope_bytes.len() > 4096
            || !hex(&descriptor.request_digest, 64)
            || descriptor.revisions.is_empty()
            || descriptor.revisions.len() > 128
            || paths.len() != 1 + 2 * descriptor.revisions.len()
        {
            return Err("invalid restored native descriptor".into());
        }
        let mut needed = std::collections::BTreeSet::new();
        let mut total = 0u64;
        for (index, revision) in descriptor.revisions.iter().enumerate() {
            let directory = scratch.path().join(index.to_string());
            let opening_bytes = read(&directory.join("opening.pb"), 512 * 1024)?;
            let originals_bytes = read(&directory.join("originals.pb"), MAX_METADATA)?;
            if digest(&opening_bytes) != revision.opening_sha256
                || digest(&originals_bytes) != revision.originals_sha256
                || revision.artifacts.len() != 2
            {
                return Err("restored native proof changed".into());
            }
            let opening = PublishContentClientFrame::decode(opening_bytes.as_slice())?;
            if open(&opening)?.checkpoint.is_some()
                || index + 1 < descriptor.revisions.len()
                    && open(&opening)?.git_acceptance.is_some()
            {
                return Err(
                    "native staging contains unexpected checkpoint or nested command".into(),
                );
            }
            if let Some(git) = &open(&opening)?.git_acceptance
                && (!git.transport_token.is_empty()
                    || git
                        .history
                        .iter()
                        .any(|r| r.open.as_ref().is_none_or(|o| o.git_acceptance.is_some())))
            {
                return Err("native staging must not retain transport secrets".into());
            }
            if index + 1 == descriptor.revisions.len() && descriptor.kind == "git-push" {
                let id = api::git_acceptance::request_digest(&opening, &descriptor.actor)
                    .ok_or("restored native command absent")?;
                if id.iter().map(|b| format!("{b:02x}")).collect::<String>()
                    != descriptor.request_digest
                {
                    return Err("restored native semantic identity changed".into());
                }
            }
            for (a, artifact) in revision.artifacts.iter().enumerate() {
                if !hex(&artifact.sha256, 64)
                    || artifact.length == 0
                    || artifact.length > MAX_NATIVE
                {
                    return Err("native artifact descriptor limit".into());
                }
                total = total
                    .checked_add(artifact.length)
                    .ok_or("native artifact size overflow")?;
                if total > MAX_NATIVE {
                    return Err("combined native restoration limit".into());
                }
                needed.insert(&artifact.sha256);
                let source = artifacts
                    .iter()
                    .find(|a| a.sha256 == artifact.sha256)
                    .ok_or("native staging artifact missing")?;
                if source.bytes.len() as u64 != artifact.length
                    || digest(&source.bytes) != artifact.sha256
                {
                    return Err("native staging artifact changed".into());
                }
                durable(&directory.join(format!("{a}.native")), &source.bytes)?;
            }
        }
        if artifacts.len() != needed.len() {
            return Err("extra or duplicate native staging artifact".into());
        }
        let directory = root.join(&descriptor.request_digest);
        if directory.exists() {
            let existing = Self::load(&directory)?;
            if canonical(&existing.descriptor)? != descriptor_bytes {
                return Err("native staging restore conflict".into());
            }
            return Ok(existing);
        }
        File::open(scratch.path())?.sync_all()?;
        fs::rename(scratch.path(), &directory)?;
        File::open(root)?.sync_all()?;
        Self::load(&directory)
    }
}

/// Complete structurally validated source, ready for the host's CURRENT authority checks.
/// `sources` retains exact proposed boundary evidence so a native installation can use the
/// ordinary authenticated hosted install path. No local owner/key/admission is invented.
pub struct HydratedPublication {
    _directory: tempfile::TempDir,
    pub source: objects::store::FsStore,
    pub sources: Vec<thread_api::publication::ProposedSourceArtifacts>,
    pub originals: Vec<crypto::thread_operation::SignedOperation>,
    pub tip: objects::object::StateId,
}
impl HydratedPublication {
    pub fn project(
        &self,
    ) -> Result<heddle_git_projection::gateway_received::ReceivedGitProjection> {
        let mut source_originals = Vec::new();
        for original in &self.originals {
            if original.verify()?.source_state()?.is_some() {
                source_originals.push(original.clone());
            }
        }
        Ok(
            heddle_git_projection::gateway_received::project_received_git_history(
                &self.source,
                &source_originals,
                self.tip,
                heddle_git_projection::gateway_view::ViewLimits::default(),
            )?,
        )
    }
}
impl StagedPublication {
    /// Validate exact pack/index bytes, every signed original and causal dependency before
    /// constructing a complete native ObjectSource. Original/current authority remains the
    /// caller's job; successful hydration never authorizes a read or an admission.
    pub fn hydrate(
        &self,
        spool_genesis: objects::object::ContentHash,
    ) -> Result<HydratedPublication> {
        use objects::{
            object::{Blob, State, StateId, Tree},
            store::{
                ObjectStore, PackReader,
                pack::{ObjectType, PackIndex, decode_tagged_entry_header},
            },
        };
        let directory = tempfile::tempdir()?;
        let store = objects::store::FsStore::new(directory.path().join("objects"));
        store.init()?;
        let mut sources = Vec::new();
        let mut originals = std::collections::BTreeMap::new();
        let mut metadata = 0u64;
        let mut decoded = 0u64;
        let mut objects_count = 0usize;
        let mut tip = None;
        for (index, descriptor) in self.descriptor.revisions.iter().enumerate() {
            let root = self.directory.join(index.to_string());
            let opening_bytes = read(&root.join("opening.pb"), 512 * 1024)?;
            let originals_bytes = read(&root.join("originals.pb"), MAX_METADATA)?;
            metadata = metadata
                .checked_add((opening_bytes.len() + originals_bytes.len()) as u64)
                .ok_or("history metadata overflow")?;
            if metadata > MAX_METADATA
                || digest(&opening_bytes) != descriptor.opening_sha256
                || digest(&originals_bytes) != descriptor.originals_sha256
            {
                return Err("native hydration metadata changed".into());
            }
            let opening = PublishContentClientFrame::decode(opening_bytes.as_slice())?;
            let mut received_originals = PublicationOriginals {
                geneses: Vec::new(),
                operations: Vec::new(),
            };
            let mut rest = originals_bytes.as_slice();
            while !rest.is_empty() {
                let frame = PublishContentClientFrame::decode_length_delimited(&mut rest)?;
                if frame.client_operation_id != opening.client_operation_id {
                    return Err("native original identity changed".into());
                }
                match frame.body {
                    Some(publish_content_client_frame::Body::ThreadGenesis(g)) => {
                        received_originals.geneses.push(g)
                    }
                    Some(publish_content_client_frame::Body::Operations(o)) => {
                        received_originals.operations.push(o)
                    }
                    _ => return Err("invalid original metadata frame".into()),
                }
            }
            let pack = self.artifact(index, 0)?;
            let packed_index = self.artifact(index, 1)?;
            let scratch = tempfile::tempdir()?;
            let reader = PackReader::from_slice(&pack, &packed_index, scratch.path())?;
            let pack_index = PackIndex::from_bytes(&packed_index)?;
            let mut offsets = std::collections::BTreeSet::new();
            for id in reader.list_ids()? {
                objects_count = objects_count
                    .checked_add(1)
                    .ok_or("history object count overflow")?;
                let offset = pack_index.find(&id)?.ok_or("history object index absent")?;
                if offsets.insert(offset) {
                    let at = usize::try_from(offset)?;
                    let header = decode_tagged_entry_header(
                        pack.get(at..).ok_or("history object offset absent")?,
                    )?;
                    decoded = decoded
                        .checked_add(header.uncompressed_size as u64)
                        .ok_or("history decoded size overflow")?;
                }
                if decoded > MAX_NATIVE || objects_count > 20_000 {
                    return Err("combined native hydration budget".into());
                }
            }
            drop(reader);
            durable(&scratch.path().join("source.pack"), &pack)?;
            durable(&scratch.path().join("source.idx"), &packed_index)?;
            let proposed = thread_api::publication::validate_proposed_source_artifacts(
                scratch,
                &opening,
                received_originals,
                spool_genesis,
            )?;
            let paths = proposed.artifacts().artifact_paths();
            let reader = PackReader::open(
                &paths[0],
                &paths[1],
                paths[0].parent().ok_or("pack parent absent")?,
            )?;
            for id in reader.list_ids()? {
                let (kind, bytes) = reader
                    .get_object(&id)?
                    .ok_or("hydrated native object absent")?;
                match kind {
                    ObjectType::Blob => {
                        store.put_blob(&Blob::new(bytes))?;
                    }
                    ObjectType::Tree => {
                        store.put_tree(&Tree::decode_canonical(&bytes)?)?;
                    }
                    ObjectType::State => {
                        store.put_state(&State::decode_current_msgpack(&bytes)?)?;
                    }
                    _ => {}
                }
            }
            for original in proposed.artifacts().operations() {
                let id = original.verify()?.id()?;
                if let Some(prior) = originals.insert(id, original.clone())
                    && prior != *original
                {
                    return Err("native original collision".into());
                }
            }
            tip = Some(proposed.artifacts().state().id());
            sources.push(proposed);
        }
        let tip: StateId = tip.ok_or("native history absent")?;
        Ok(HydratedPublication {
            _directory: directory,
            source: store,
            sources,
            originals: originals.into_values().collect(),
            tip,
        })
    }
}

impl StagedPublication {
    pub fn actor(&self) -> &str {
        &self.descriptor.actor
    }
    pub fn openings(&self) -> Result<Vec<PublishContentClientFrame>> {
        let mut result = Vec::new();
        for (index, descriptor) in self.descriptor.revisions.iter().enumerate() {
            let bytes = read(
                &self.directory.join(index.to_string()).join("opening.pb"),
                512 * 1024,
            )?;
            if digest(&bytes) != descriptor.opening_sha256 {
                return Err("native stage opening changed".into());
            }
            result.push(PublishContentClientFrame::decode(bytes.as_slice())?);
        }
        Ok(result)
    }
    pub fn scope(&self) -> Result<GitTransportScope> {
        Ok(GitTransportScope::decode(
            self.descriptor.scope_bytes.as_slice(),
        )?)
    }
    pub fn is_bootstrap(&self) -> bool {
        self.descriptor.kind == "native-bootstrap"
    }
    pub fn accepted_receipt(&self) -> Result<PublicationReceipt> {
        if self.is_bootstrap() {
            return Err("bootstrap has no Git acceptance receipt".into());
        }
        let bytes = read(&self.directory.join("accepted.pb"), MAX_METADATA)?;
        let receipt = PublicationReceipt::decode(bytes.as_slice())?;
        let accepted = receipt
            .git_acceptance
            .as_ref()
            .ok_or("durable native acceptance absent")?;
        if accepted
            .request_digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            != self.descriptor.request_digest
            || accepted.actor_account_id != self.descriptor.actor
            || !matches!(
                receipt.outcome,
                Some(publication_receipt::Outcome::Accepted(_))
            )
        {
            return Err("durable acceptance differs from staged native intent".into());
        }
        Ok(receipt)
    }
}

fn bootstrap_identity(
    actor: &str,
    scope: &[u8],
    openings: &[PublishContentClientFrame],
) -> Result<String> {
    let mut bytes = b"heddle-native-bootstrap-stage-v1\0".to_vec();
    for part in [actor.as_bytes(), scope] {
        bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
        bytes.extend_from_slice(part);
    }
    for frame in openings {
        let encoded = frame.encode_to_vec();
        if bytes.len() + encoded.len() > MAX_METADATA as usize {
            return Err("bootstrap metadata limit".into());
        }
        bytes.extend_from_slice(&(encoded.len() as u64).to_le_bytes());
        bytes.extend(encoded);
    }
    Ok(digest(&bytes))
}
impl StagedPublication {
    fn verify_identity(&self) -> Result<()> {
        let frames = self.openings()?;
        let scope = self.scope()?;
        if scope.encode_to_vec() != self.descriptor.scope_bytes {
            return Err("noncanonical native scope".into());
        }
        if self.is_bootstrap() {
            if frames.iter().any(|f| {
                open(f).is_ok_and(|o| o.git_acceptance.is_some() || o.checkpoint.is_some())
            }) || bootstrap_identity(
                &self.descriptor.actor,
                &self.descriptor.scope_bytes,
                &frames,
            )? != self.descriptor.request_digest
            {
                return Err("bootstrap identity changed".into());
            }
        } else {
            let tip = frames.last().ok_or("native tip absent")?;
            let git = open(tip)?
                .git_acceptance
                .as_ref()
                .ok_or("native command absent")?;
            let expected = api::git_acceptance::request_digest(tip, &self.descriptor.actor)
                .ok_or("native request absent")?;
            if git.scope.as_ref() != Some(&scope)
                || !git.transport_token.is_empty()
                || expected
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
                    != self.descriptor.request_digest
            {
                return Err("native command identity changed".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "native_publication_tests.rs"]
mod native_publication_tests;

/// Token-free metadata identity, available before downloading source artifacts. This is not
/// native admission or source disclosure authority; the host queries Weft using the original
/// tip operation UUID and compares its actual receipt before trusting any accepted state.
pub struct InspectedProof {
    pub actor: String,
    pub scope: GitTransportScope,
    pub request_digest: String,
    pub bootstrap: bool,
    pub openings: Vec<PublishContentClientFrame>,
}
impl StagedPublication {
    pub fn inspect_proof(proof: &[u8]) -> Result<InspectedProof> {
        if proof.is_empty() || proof.len() as u64 > MAX_METADATA + 1024 * 1024 {
            return Err("native proof archive limit".into());
        }
        let mut files = std::collections::BTreeMap::new();
        let mut total = 0u64;
        for entry in tar::Archive::new(proof).entries()?.raw(true) {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let path = path.to_str().ok_or("native proof path encoding")?;
            let allowed = path == "manifest.json"
                || path.split_once('/').is_some_and(|(index, file)| {
                    index
                        .parse::<usize>()
                        .is_ok_and(|n| n < 128 && n.to_string() == index)
                        && matches!(file, "opening.pb" | "originals.pb")
                });
            total = total
                .checked_add(entry.size())
                .ok_or("native proof overflow")?;
            if !allowed
                || !entry.header().entry_type().is_file()
                || files.contains_key(path)
                || total > MAX_METADATA + 64 * 1024
            {
                return Err("native proof path or metadata limit".into());
            }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            files.insert(path.to_string(), bytes);
        }
        let bytes = files
            .get("manifest.json")
            .ok_or("native descriptor missing")?;
        if bytes.len() > 64 * 1024 {
            return Err("native descriptor limit".into());
        }
        let descriptor: Descriptor = serde_json::from_slice(bytes)?;
        let actor = uuid::Uuid::parse_str(&descriptor.actor)?;
        if canonical(&descriptor)? != *bytes
            || descriptor.schema != 1
            || actor.is_nil()
            || actor.to_string() != descriptor.actor
            || !matches!(descriptor.kind.as_str(), "git-push" | "native-bootstrap")
            || !hex(&descriptor.request_digest, 64)
            || descriptor.scope_bytes.is_empty()
            || descriptor.scope_bytes.len() > 4096
            || descriptor.revisions.is_empty()
            || descriptor.revisions.len() > 128
            || files.len() != 1 + 2 * descriptor.revisions.len()
        {
            return Err("native proof descriptor invalid".into());
        }
        let scope = GitTransportScope::decode(descriptor.scope_bytes.as_slice())?;
        if scope.encode_to_vec() != descriptor.scope_bytes
            || scope.service_audience != "git-gateway"
            || scope.thread.as_ref().is_none_or(|t| t.value.len() != 32)
            || ![1, 2].contains(&scope.action)
        {
            return Err("native proof scope invalid".into());
        }
        let mut openings = Vec::new();
        let mut native_bytes = 0u64;
        let mut proof_originals = PublicationOriginals {
            geneses: Vec::new(),
            operations: Vec::new(),
        };
        for (index, revision) in descriptor.revisions.iter().enumerate() {
            let encoded = files
                .get(&format!("{index}/opening.pb"))
                .ok_or("native opening missing")?;
            let originals = files
                .get(&format!("{index}/originals.pb"))
                .ok_or("native originals missing")?;
            if encoded.len() > 512 * 1024
                || digest(encoded) != revision.opening_sha256
                || digest(originals) != revision.originals_sha256
                || revision.artifacts.len() != 2
            {
                return Err("native proof metadata changed".into());
            }
            for artifact in &revision.artifacts {
                native_bytes = native_bytes
                    .checked_add(artifact.length)
                    .ok_or("native source overflow")?;
                if !hex(&artifact.sha256, 64) || artifact.length == 0 || native_bytes > MAX_NATIVE {
                    return Err("native source proof limit".into());
                }
            }
            let frame = PublishContentClientFrame::decode(encoded.as_slice())?;
            thread_api::hybrid::publish_open(open(&frame)?)
                .map_err(|_| "invalid native proof opening")?;
            if frame.encode_to_vec() != *encoded
                || open(&frame)?.checkpoint.is_some()
                || index + 1 < descriptor.revisions.len() && open(&frame)?.git_acceptance.is_some()
            {
                return Err("noncanonical or nested native proof".into());
            }
            let mut remaining = originals.as_slice();
            while !remaining.is_empty() {
                let before = remaining;
                let original = PublishContentClientFrame::decode_length_delimited(&mut remaining)?;
                if original.encode_length_delimited_to_vec()
                    != before[..before.len() - remaining.len()]
                    || original.client_operation_id != frame.client_operation_id
                    || !matches!(
                        original.body,
                        Some(
                            publish_content_client_frame::Body::ThreadGenesis(_)
                                | publish_content_client_frame::Body::Operations(_)
                        )
                    )
                {
                    return Err("unexpected native original frame".into());
                }
                if let Some(publish_content_client_frame::Body::Operations(batch)) = original.body {
                    proof_originals.operations.push(batch);
                }
            }
            openings.push(frame);
        }
        let bootstrap = descriptor.kind == "native-bootstrap";
        if bootstrap {
            if openings
                .iter()
                .any(|f| open(f).is_ok_and(|o| o.git_acceptance.is_some()))
                || bootstrap_identity(&descriptor.actor, &descriptor.scope_bytes, &openings)?
                    != descriptor.request_digest
            {
                return Err("native bootstrap proof identity changed".into());
            }
        } else {
            let tip = openings.last().ok_or("native tip absent")?;
            let git = open(tip)?
                .git_acceptance
                .as_ref()
                .ok_or("native Git command absent")?;
            let expected = api::git_acceptance::request_digest(tip, &descriptor.actor)
                .ok_or("native request identity absent")?;
            if !git.transport_token.is_empty()
                || git.expected_native_generation.is_none_or(|n| n < 0)
                || thread_api::publication::git_originals_digest([&proof_originals])?.as_slice()
                    != git.originals_digest
                || scope.action != GitTransportAction::Write as i32
                || git.scope.as_ref() != Some(&scope)
                || git.history.len() + 1 != openings.len()
                || expected
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
                    != descriptor.request_digest
            {
                return Err("native Git proof identity changed or secret present".into());
            }
            for (child, frame) in git.history.iter().zip(&openings) {
                if child.client_operation_id != frame.client_operation_id
                    || child.open.as_ref() != Some(open(frame)?)
                {
                    return Err("native history proof differs from exact command".into());
                }
            }
        }
        // Reject alternate TAR headers, ignored extension records and bytes after end markers.
        // The proof is a canonical private format, not a permissive general TAR import.
        let mut archive = tar::Builder::new(Vec::new());
        let mut paths = vec!["manifest.json".to_string()];
        for index in 0..descriptor.revisions.len() {
            paths.push(format!("{index}/opening.pb"));
            paths.push(format!("{index}/originals.pb"));
        }
        for path in paths {
            let bytes = files.get(&path).ok_or("native proof file absent")?;
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_mtime(0);
            header.set_cksum();
            archive.append_data(&mut header, path, bytes.as_slice())?;
        }
        if archive.into_inner()? != proof {
            return Err("noncanonical native proof archive".into());
        }
        Ok(InspectedProof {
            actor: descriptor.actor,
            scope,
            request_digest: descriptor.request_digest,
            bootstrap,
            openings,
        })
    }
}
