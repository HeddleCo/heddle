//! Explicit source-head selection, retaining every competing head as ancestry.
use std::collections::BTreeSet;

use crypto::{Signer, thread_operation::SignedOperation};
use objects::{
    object::{
        Attribution, ContentHash, State, StateId, StateVisibility, VisibilityTier,
        thread_replication::{ThreadFacet, ThreadOperation, ThreadOperationBody},
    },
    store::ObjectStore,
};
use serde::{Deserialize, Serialize};

use super::{Admission, Error, Result, ThreadReplica, checkout::ThreadCheckout};

#[derive(Serialize, Deserialize)]
struct Resolution {
    expected: StateId,
    selected: StateId,
    conflict_version: Vec<u8>,
    actor: Vec<u8>,
    attribution: Attribution,
    operation: Vec<u8>,
}
pub fn source_conflict_version(thread: ContentHash, heads: &BTreeSet<StateId>) -> Vec<u8> {
    let mut bytes = thread.as_bytes().to_vec();
    for head in heads {
        bytes.extend_from_slice(head.as_bytes());
    }
    ContentHash::compute_typed("heddle-source-conflicts-v2", &bytes)
        .as_bytes()
        .to_vec()
}
impl ThreadCheckout {
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_source_choice(
        &self,
        replica: &ThreadReplica,
        lease: &str,
        token: &str,
        operation_id: &str,
        expected: StateId,
        conflict_version: &[u8],
        selected: StateId,
        attribution: Attribution,
        signer: &impl Signer,
    ) -> Result<SignedOperation> {
        uuid::Uuid::parse_str(operation_id).map_err(|e| Error::Invalid(e.to_string()))?;
        let _writer =
            self.repository
                .authenticate_checkout_writer(self.binding.thread, lease, token)?;
        if replica.thread_id() != self.binding.thread {
            return Err(Error::Invalid("resolution targets another Thread".into()));
        }
        let directory = self.repository.root().join(".heddle/source-resolutions");
        objects::fs_atomic::create_dir_all_durable(&directory)?;
        let path = directory.join(format!("{operation_id}.json"));
        let journal = if path.exists() {
            let journal: Resolution = serde_json::from_slice(&std::fs::read(&path)?)
                .map_err(|e| Error::Invalid(e.to_string()))?;
            if journal.expected != expected
                || journal.selected != selected
                || journal.conflict_version != conflict_version
                || journal.actor != signer.public_key()
                || journal.attribution != attribution
            {
                return Err(Error::Invalid(
                    "resolution retry changes signed intent".into(),
                ));
            }
            journal
        } else {
            if self.repository.head()? != Some(expected)
                || !self.repository.worktree_matches_state(&expected)?
            {
                return Err(Error::Invalid(
                    "resolution requires unchanged clean checkout".into(),
                ));
            }
            let view = replica.view()?;
            if view.source_heads.len() < 2
                || !view.source_heads.contains(&selected)
                || source_conflict_version(self.binding.thread, &view.source_heads)
                    != conflict_version
            {
                return Err(Error::Invalid("source conflict candidates changed".into()));
            }
            let source = self
                .repository
                .store()
                .get_state(&selected)?
                .ok_or_else(|| Error::Invalid("selected source unavailable".into()))?;
            let state = State::new_merge(
                source.tree,
                view.source_heads.iter().copied().collect(),
                attribution.clone(),
            )
            .with_intent("Explicit source-head resolution");
            let operation = ThreadOperation {
                version: 1,
                thread: self.binding.thread,
                parents: view
                    .frontiers
                    .get(&ThreadFacet::Source)
                    .cloned()
                    .unwrap_or_default(),
                publisher: signer
                    .public_key()
                    .try_into()
                    .map_err(|_| Error::Invalid("invalid source publisher".into()))?,
                body: ThreadOperationBody::Capture(
                    objects::object::thread_replication::AuthoredCapture {
                        result: replica.prepare_capture(&self.repository, &state)?,
                        author: replica.source_author_for(
                            &signer
                                .public_key()
                                .try_into()
                                .map_err(|_| Error::Invalid("source signer length".into()))?,
                        )?,
                    },
                ),
            };
            let journal = Resolution {
                expected,
                selected,
                conflict_version: conflict_version.to_vec(),
                actor: signer.public_key().to_vec(),
                attribution: attribution.clone(),
                operation: operation.encode()?,
            };
            objects::fs_atomic::write_file_atomic_secret(
                &path,
                &serde_json::to_vec(&journal).map_err(|e| Error::Invalid(e.to_string()))?,
            )?;
            journal
        };
        let operation = ThreadOperation::decode(&journal.operation)?;
        let state = operation
            .source_state()?
            .ok_or_else(|| Error::Invalid("resolution result missing".into()))?;
        let signed = SignedOperation::sign(&operation, signer)?;
        if replica.accepted_source_revision(state.id())?.is_some() {
            return Ok(signed);
        }
        let head = self
            .repository
            .head()?
            .ok_or_else(|| Error::Invalid("checkout source missing".into()))?;
        if head != expected && head != state.id() {
            return Err(Error::Invalid(
                "checkout changed during resolution recovery".into(),
            ));
        }
        if head == expected {
            if !self.repository.worktree_matches_state(&expected)?
                && !self.repository.worktree_matches_state(&selected)?
            {
                return Err(Error::Invalid(
                    "working edits prevent source resolution".into(),
                ));
            }
            self.repository.put_authored_state(&state)?;
            let mut tier = self.repository.resolve_capture_default_visibility();
            for parent in &state.parents {
                tier =
                    objects::object::thread_replication::local_integration::intersect_visibility(
                        &tier,
                        &self
                            .repository
                            .effective_visibility_tier(parent)
                            .map_err(|e| Error::Invalid(e.to_string()))?,
                    )?;
            }
            if tier != VisibilityTier::Public {
                self.repository
                    .put_state_visibility_if_absent(StateVisibility {
                        state: state.id(),
                        tier,
                        embargo_until: None,
                        declarer: attribution.principal,
                        declared_at: state.created_at,
                        signature: None,
                        supersedes: None,
                    })
                    .map_err(|e| Error::Invalid(e.to_string()))?;
            }
            self.repository
                .restore_worktree_state_only(&state.id(), Some(&expected))?;
            self.repository
                .commit_snapshot_atomic_with_capture_visibility(
                    &state.id(),
                    Some(expected),
                    None,
                    false,
                )?;
        }
        if replica.receive_prepared_source(&signed, self.repository.store(), |_| Ok(()))?
            != Admission::Accepted
        {
            return Err(Error::Invalid(
                "resolution source ancestry incomplete".into(),
            ));
        }
        Ok(signed)
    }
}
