// SPDX-License-Identifier: Apache-2.0
//! Checkout-local capture. Shared Thread observations never move this HEAD.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use chrono::Utc;
use crypto::{Signer, thread_operation::SignedOperation};
use objects::{
    object::{
        Attribution, ContentHash, StateId,
        thread_replication::{ThreadOperation, ThreadOperationBody},
    },
    store::{
        ObjectStore, WriterLeaseDraft, WriterLeaseGrant, WriterLeaseReserveOutcome,
        WriterLeaseStore,
    },
};
use refs::Head;
use serde::{Deserialize, Serialize};

use super::{Admission, Error, Result, ThreadReplica};
use crate::{AudienceTier, CheckoutMaterialization, Repository};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckoutBinding {
    pub id: String,
    pub thread: ContentHash,
}
#[derive(Serialize, Deserialize)]
struct CaptureJournal {
    operation_id: String,
    expected: StateId,
    summary: String,
    publisher: Vec<u8>,
    parents: BTreeSet<ContentHash>,
    resulting: Option<StateId>,
    attribution: Attribution,
    #[serde(default)]
    selection: Vec<String>,
    capture: Option<objects::object::thread_replication::AuthoredCapture>,
}

#[derive(Clone)]
pub struct CaptureInput<'a> {
    pub lease: &'a str,
    pub token: &'a str,
    pub operation_id: &'a str,
    pub expected: StateId,
    pub summary: &'a str,
    pub attribution: Attribution,
}

pub struct ThreadCheckout {
    pub binding: CheckoutBinding,
    pub repository: Repository,
    local_dir: PathBuf,
}
impl ThreadCheckout {
    /// Called under device authority. The destination must be absent; no
    /// existing checkout is repurposed or overwritten.
    pub fn create(
        source: &Repository,
        replica: &ThreadReplica,
        path: &Path,
        revision: StateId,
        audience: &AudienceTier,
    ) -> Result<Self> {
        if path.exists() {
            return Err(Error::Invalid("checkout destination already exists".into()));
        }
        if revision != replica.genesis()?.base
            && replica.accepted_source_revision(revision)?.is_none()
        {
            return Err(Error::Invalid(
                "checkout source is not admitted in this Thread".into(),
            ));
        }
        let state = source
            .store()
            .get_state(&revision)?
            .ok_or_else(|| Error::Invalid("checkout source unavailable".into()))?;
        match source.checkout_state_gated(&revision, &state, path, audience)? {
            CheckoutMaterialization::Materialized { .. } => {}
            _ => {
                return Err(Error::Invalid(
                    "checkout source is outside the audience".into(),
                ));
            }
        }
        Repository::init_worktree(path, source.heddle_dir())?;
        let repository = Repository::open(path)?;
        repository
            .refs()
            .write_head(&Head::Detached { state: revision })?;
        let binding = CheckoutBinding {
            id: uuid::Uuid::now_v7().to_string(),
            thread: replica.thread_id(),
        };
        let local_dir = path.canonicalize()?.join(".heddle");
        objects::fs_atomic::write_file_atomic(
            &local_dir.join("thread-checkout.json"),
            &serde_json::to_vec(&binding).map_err(|e| Error::Invalid(e.to_string()))?,
        )?;
        let index = source.heddle_dir().join("native-checkouts");
        objects::fs_atomic::create_dir_all_durable(&index)?;
        objects::fs_atomic::write_file_atomic_secret(
            &index.join(format!("{}.json", binding.id)),
            &serde_json::to_vec(&path.canonicalize()?)
                .map_err(|error| Error::Invalid(error.to_string()))?,
        )?;
        Ok(Self {
            binding,
            repository,
            local_dir,
        })
    }
    pub fn open(path: &Path) -> Result<Self> {
        let local_dir = path.canonicalize()?.join(".heddle");
        let binding =
            serde_json::from_slice(&std::fs::read(local_dir.join("thread-checkout.json"))?)
                .map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(Self {
            binding,
            repository: Repository::open(path)?,
            local_dir,
        })
    }
    pub fn claim_writer(&self, actor: String, pid: Option<u32>) -> Result<WriterLeaseGrant> {
        let result = WriterLeaseStore::new(self.repository.heddle_dir()).reserve(
            WriterLeaseDraft {
                thread: self.binding.thread.to_hex(),
                actor_session_id: Some(actor),
                task_assignment_id: None,
                anchor_state: self.repository.head()?.map(|id| id.to_string_full()),
                anchor_root: None,
                path: Some(self.repository.root().to_owned()),
                pid,
                boot_id: pid.and_then(|_| objects::store::current_boot_id()),
            },
            Utc::now(),
        )?;
        match result {
            WriterLeaseReserveOutcome::Reserved(grant) => Ok(grant),
            WriterLeaseReserveOutcome::LiveOwner(owner) => Err(Error::Invalid(format!(
                "checkout already has writer {}",
                owner.lease_id
            ))),
        }
    }

    /// A journal binds the retry ID before native capture. If the process stops
    /// between native commit and Thread admission, the checkout's detached HEAD
    /// recovers the exact capture; a retry never takes a second snapshot.
    pub fn capture(
        &self,
        replica: &ThreadReplica,
        input: CaptureInput<'_>,
        signer: &impl Signer,
    ) -> Result<SignedOperation> {
        self.capture_with_paths(replica, input, signer, &[])
    }
    pub fn capture_with_paths(
        &self,
        replica: &ThreadReplica,
        input: CaptureInput<'_>,
        signer: &impl Signer,
        selected_paths: &[String],
    ) -> Result<SignedOperation> {
        let mut selection = selected_paths.to_vec();
        selection.sort();
        selection.dedup();
        if selection.len() > 256
            || selection.iter().any(|path| {
                path.len() > 4096
                    || path.split('/').count() > 128
                    || path.is_empty()
                    || std::path::Path::new(path)
                        .components()
                        .any(|part| !matches!(part, std::path::Component::Normal(_)))
            })
        {
            return Err(Error::Invalid(
                "capture selection must contain repository-relative paths".into(),
            ));
        }
        let CaptureInput {
            lease,
            token,
            operation_id,
            expected,
            summary,
            attribution,
        } = &input;
        let expected = *expected;
        if operation_id.is_empty() || replica.thread_id() != self.binding.thread {
            return Err(Error::Invalid(
                "invalid capture scope or operation ID".into(),
            ));
        }
        let lock = objects::lock::RepoLock::at(self.local_dir.join("thread-capture.lock"));
        let _guard = lock.write().map_err(|e| Error::Invalid(e.to_string()))?;
        let _writer =
            self.repository
                .authenticate_checkout_writer(self.binding.thread, lease, token)?;
        let path = self.local_dir.join("capture-journal.json");
        let receipts = self.local_dir.join("capture-receipts");
        objects::fs_atomic::create_dir_all_durable(&receipts)?;
        let completed_path = receipts.join(
            ContentHash::compute_typed("checkout-command-v1", operation_id.as_bytes()).to_hex(),
        );
        let completed = match std::fs::read(&completed_path) {
            Ok(bytes) => Some(
                serde_json::from_slice::<CaptureJournal>(&bytes)
                    .map_err(|e| Error::Invalid(e.to_string()))?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(completed) = completed {
            if completed.operation_id != *operation_id
                || completed.expected != expected
                || completed.summary != *summary
                || completed.attribution != *attribution
                || completed.publisher != signer.public_key()
                || completed.selection != selection
            {
                return Err(Error::Invalid(
                    "capture retry changes its signed inputs".into(),
                ));
            }
            let state_id = completed
                .resulting
                .ok_or_else(|| Error::Invalid("capture receipt has no result".into()))?;
            let state = self
                .repository
                .store()
                .get_state(&state_id)?
                .ok_or_else(|| Error::Invalid("captured state unavailable".into()))?;
            let canonical_state = state.encode_current_msgpack()?;
            if completed
                .capture
                .as_ref()
                .is_none_or(|capture| capture.result.state != canonical_state)
            {
                return Err(Error::Invalid(
                    "capture receipt source differs from committed State".into(),
                ));
            }
            // The receipt is written first. Recover a stop between the two
            // durable writes, without disturbing a later command's journal.
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let active: CaptureJournal = serde_json::from_slice(&bytes)
                        .map_err(|e| Error::Invalid(e.to_string()))?;
                    if active.operation_id == completed.operation_id {
                        write_journal(&path, &completed)?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    write_journal(&path, &completed)?;
                }
                Err(error) => return Err(error.into()),
            }
            return Ok(SignedOperation::sign(
                &ThreadOperation {
                    version: 1,
                    thread: self.binding.thread,
                    parents: completed.parents,
                    publisher: signer
                        .public_key()
                        .try_into()
                        .map_err(|_| Error::Invalid("publisher key must be Ed25519".into()))?,
                    body: ThreadOperationBody::Capture(completed.capture.clone().ok_or_else(
                        || Error::Invalid("capture receipt missing signed descriptor".into()),
                    )?),
                },
                signer,
            )?);
        }
        let old = match std::fs::read(&path) {
            Ok(bytes) => Some(
                serde_json::from_slice::<CaptureJournal>(&bytes)
                    .map_err(|e| Error::Invalid(e.to_string()))?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let mut journal = if let Some(old) = old {
            if old.operation_id == *operation_id {
                if old.expected != expected
                    || old.summary != *summary
                    || old.publisher != signer.public_key()
                    || old.attribution != *attribution
                    || old.selection != selection
                {
                    return Err(Error::Invalid(
                        "capture retry changes its signed inputs".into(),
                    ));
                }
                old
            } else {
                if old.resulting.is_none() {
                    return Err(Error::Invalid(format!(
                        "recover pending capture {} before starting another",
                        old.operation_id
                    )));
                }
                self.new_journal(replica, &input, signer, &selection)?
            }
        } else {
            self.new_journal(replica, &input, signer, &selection)?
        };
        write_journal(&path, &journal)?;
        let state = if let Some(id) = journal.resulting {
            self.repository
                .store()
                .get_state(&id)?
                .ok_or_else(|| Error::Invalid("captured state unavailable".into()))?
        } else {
            let head = self
                .repository
                .head()?
                .ok_or_else(|| Error::Invalid("checkout has no HEAD".into()))?;
            if head != expected {
                let state = self
                    .repository
                    .store()
                    .get_state(&head)?
                    .ok_or_else(|| Error::Invalid("recovery source unavailable".into()))?;
                if state.parents != vec![expected]
                    || state.intent.as_deref() != Some(*summary)
                    || state.attribution != *attribution
                {
                    return Err(Error::Invalid(
                        "checkout changed outside the pending capture; explicit recovery required"
                            .into(),
                    ));
                }
                state
            } else {
                if selection.is_empty() {
                    self.repository.snapshot_with_attribution(
                        Some((*summary).to_owned()),
                        None,
                        attribution.clone(),
                    )?
                } else {
                    let baseline = self
                        .repository
                        .store()
                        .get_state(&expected)?
                        .ok_or_else(|| Error::Invalid("capture parent missing".into()))?;
                    let tree = self
                        .repository
                        .store()
                        .get_tree(&baseline.tree)?
                        .ok_or_else(|| Error::Invalid("capture tree missing".into()))?;
                    let working = self.repository.build_tree(self.repository.root())?;
                    let tree = super::checkout_selection::selected_tree(
                        self.repository.store(),
                        tree,
                        &working,
                        &selection,
                    )?;
                    self.repository
                        .snapshot_tree_with_attribution_profiled(
                            tree,
                            Some((*summary).to_owned()),
                            None,
                            attribution.clone(),
                        )?
                        .state
                }
            }
        };
        let capture = match &journal.capture {
            Some(capture) => capture.clone(),
            None => {
                let capture = objects::object::thread_replication::AuthoredCapture {
                    result: replica.prepare_capture(&self.repository, &state)?,
                    author: self.repository.native_capture_author(
                        &signer
                            .public_key()
                            .try_into()
                            .map_err(|_| Error::Invalid("source signer length".into()))?,
                        uuid::Uuid::parse_str(&replica.genesis()?.spool)
                            .map_err(|error| Error::Invalid(error.to_string()))?,
                    )?,
                };
                journal.capture = Some(capture.clone());
                journal.resulting = Some(state.id());
                write_journal(&path, &journal)?;
                capture
            }
        };
        let operation = ThreadOperation {
            version: 1,
            thread: self.binding.thread,
            parents: journal.parents.clone(),
            publisher: signer
                .public_key()
                .try_into()
                .map_err(|_| Error::Invalid("publisher key must be Ed25519".into()))?,
            body: ThreadOperationBody::Capture(capture),
        };
        let signed = SignedOperation::sign(&operation, signer)?;
        if replica.receive_prepared_source(&signed, self.repository.store(), |_| Ok(()))?
            != Admission::Accepted
        {
            return Err(Error::Invalid(
                "local capture has incomplete causal ancestry".into(),
            ));
        }
        journal.resulting = Some(state.id());
        write_journal(&completed_path, &journal)?;
        write_journal(&path, &journal)?;
        Ok(signed)
    }

    fn new_journal(
        &self,
        replica: &ThreadReplica,
        input: &CaptureInput<'_>,
        signer: &impl Signer,
        selection: &[String],
    ) -> Result<CaptureJournal> {
        let expected = input.expected;
        if self.repository.head()? != Some(expected) {
            return Err(Error::Invalid("stale checkout source".into()));
        }
        let mut parents = BTreeSet::new();
        if expected != replica.genesis()?.base {
            let mut cursor = None;
            loop {
                let page = replica.source_operation_page(expected, cursor, 128)?;
                if page.is_empty() {
                    break;
                }
                cursor = page.last().copied();
                parents.extend(page);
            }
            if parents.is_empty() {
                return Err(Error::Invalid(
                    "checkout source is not admitted in this Thread".into(),
                ));
            }
        }
        Ok(CaptureJournal {
            operation_id: input.operation_id.into(),
            expected,
            summary: input.summary.into(),
            publisher: signer.public_key().to_vec(),
            parents,
            resulting: None,
            capture: None,
            attribution: input.attribution.clone(),
            selection: selection.to_vec(),
        })
    }
}
fn write_journal(path: &Path, journal: &CaptureJournal) -> Result<()> {
    let bytes = serde_json::to_vec(journal).map_err(|e| Error::Invalid(e.to_string()))?;
    objects::fs_atomic::write_file_atomic(path, &bytes)?;
    Ok(())
}
