// SPDX-License-Identifier: Apache-2.0
//! Immutable per-revision preparation, explicit acceptance, and fresh disclosure.
use super::*;
impl PreparedRevision {
    pub fn source(&self) -> &SourcePack {
        &self.source
    }
    pub fn publication(&self) -> &PreparedPublication {
        &self.publication
    }
    /// Explicit caller signing can attach an account acceptance without changing
    /// original author claims. No signer lookup or identity refresh occurs here.
    pub fn sign_acceptance(
        &mut self,
        author: objects::object::thread_replication::SourceAuthor,
        kinds: std::collections::BTreeSet<
            objects::object::original_boundary_acceptance::BoundaryOriginalKind,
        >,
        signer: &impl Signer,
    ) -> GitProjectionResult<ContentHash> {
        let publisher = signer
            .public_key()
            .try_into()
            .map_err(|_| failure("accepting key width"))?;
        let value = self
            .publication
            .acceptance(author, publisher, kinds)
            .map_err(failure)?;
        self.accept(SignedBoundaryAcceptance::sign(&value, signer).map_err(failure)?)
    }
    /// Attach an external exact-intent signature within the whole-history budget.
    pub fn accept(&mut self, signed: SignedBoundaryAcceptance) -> GitProjectionResult<ContentHash> {
        if signed.canonical.len() > 96 * 1024 || signed.signature.len() != 64 {
            return Err(failure("bounded boundary acceptance required"));
        }
        let bytes = signed.canonical.len() + signed.signature.len() + 512;
        self.metadata_remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(bytes)
            })
            .map_err(|_| failure("complete history proposal metadata limit"))?;
        match self.publication.accept(signed).map_err(failure) {
            Ok(id) => Ok(id),
            Err(error) => {
                self.metadata_remaining.fetch_add(bytes, Ordering::Relaxed);
                Err(error)
            }
        }
    }
    /// Validate files actually received in an isolated directory. This invokes
    /// the shared receiver's full pack/signature/causal-closure validator, not a
    /// predicted digest. It grants no authority and performs no durable commit.
    pub fn validate_received(
        &self,
        directory: tempfile::TempDir,
        spool_genesis: ContentHash,
    ) -> GitProjectionResult<thread_api::publication::ProposedSourceArtifacts> {
        if spool_genesis != self.publication.plan().intent().spool_genesis {
            return Err(failure(
                "receiver Spool genesis differs from exact publication intent",
            ));
        }
        thread_api::publication::validate_proposed_source_artifacts(
            directory,
            self.publication.opening(),
            self.publication.originals().clone(),
            spool_genesis,
        )
        .map_err(failure)
    }
    /// One explicit exchange to the independently authenticated endpoint. This
    /// returns only a byte-publication receipt. Callers must obtain receiver CAS
    /// acceptance and finish recoverable catalog publication before Git ACK.
    /// `authorize` checks current disclosure for the exact original ancestry,
    /// endpoints and policy, returning a caller-owned guard held through send.
    /// Preparation or a prior successful check never supplies this permission.
    pub async fn send<T: RpcTransport<Error = transport::Error>, G>(
        &self,
        remote: &Remote<T>,
        authorize: impl FnOnce(&PreparedPublication) -> GitProjectionResult<G>,
    ) -> GitProjectionResult<PublicationReceipt> {
        let _disclosure_guard = authorize(&self.publication)?;
        let Some(publish_content_client_frame::Body::Open(open)) = &self.publication.opening().body
        else {
            return Err(failure("publication opening absent"));
        };
        let reference = open
            .thread
            .clone()
            .ok_or_else(|| failure("Thread absent"))?;
        remote
            .thread(reference)
            .send_prepared(&self.source, &self.publication)
            .await
            .map_err(failure)
    }
}

/// The Git actor's verified session is separate from the native gateway publisher.
/// These values come from current authority, never Git author text or forwarding headers.
pub struct GitAcceptanceActor<'a> {
    pub transport_token: &'a str,
    pub account_id: &'a str,
    pub scope: &'a GitTransportScope,
}
impl PreparedHistory {
    /// Send the complete exact history in one receiver transaction, preserving the original
    /// per-revision account acceptance statements. Returns native acceptance only; the host
    /// must durably publish R2 + the Artifacts catalog and reauthorize before acknowledging Git.
    pub async fn send_git<T: RpcTransport<Error = transport::Error>, G>(
        &self,
        remote: &Remote<T>,
        push: &crate::gateway_write::SignedGitPush,
        actor: GitAcceptanceActor<'_>,
        expected_native_generation: i64,
        authorize: impl FnOnce(&crate::gateway_write::PushScope, &[StateId]) -> GitProjectionResult<G>,
    ) -> GitProjectionResult<PublicationReceipt> {
        let scope = push.scope();
        if expected_native_generation < 0
            || self.tip != push.receipt().native_state
            || self
                .revisions
                .last()
                .is_none_or(|r| r.source.revision() != self.tip)
            || scope.actor != actor.account_id
            || actor.transport_token.is_empty()
        {
            return Err(failure(
                "verified Git actor and exact prepared history required",
            ));
        }
        let states = self
            .revisions
            .iter()
            .map(|r| r.source.revision())
            .collect::<Vec<_>>();
        let _guard = authorize(scope, &states)?;
        let mut uploads = Vec::new();
        for revision in &self.revisions {
            uploads.push(thread_api::publication::GitHistoryUpload {
                opening: revision.publication.opening().clone(),
                originals: revision.publication.originals().clone(),
                artifacts: revision.source.open_artifacts().await.map_err(failure)?,
            });
        }
        let mut tip = uploads
            .pop()
            .ok_or_else(|| failure("Git tip source absent"))?;
        let originals_digest = thread_api::publication::git_originals_digest(
            uploads
                .iter()
                .chain(std::iter::once(&tip))
                .map(|r| &r.originals),
        )
        .map_err(failure)?;
        let histories = uploads
            .iter()
            .map(|revision| {
                let Some(publish_content_client_frame::Body::Open(open)) = &revision.opening.body
                else {
                    return Err(failure("historical source opening absent"));
                };
                Ok(GitPushHistoryRevision {
                    client_operation_id: revision.opening.client_operation_id.clone(),
                    open: Some(open.clone()),
                })
            })
            .collect::<GitProjectionResult<Vec<_>>>()?;
        let old: sley::ObjectId = scope.old_git.parse().map_err(failure)?;
        let new: sley::ObjectId = scope.new_git.parse().map_err(failure)?;
        let Some(publish_content_client_frame::Body::Open(open)) = &mut tip.opening.body else {
            return Err(failure("tip source opening absent"));
        };
        if open.git_acceptance.is_some()
            || open.thread.as_ref().is_none_or(|t| {
                t.spool.as_ref().is_none_or(|s| s.id != scope.spool)
                    || t.id
                        .as_ref()
                        .is_none_or(|id| id.value != scope.thread_id.as_bytes())
            })
        {
            return Err(failure(
                "Git acceptance scope differs from source publication",
            ));
        }
        open.protocol = Some(api::heddle::api::common::ProtocolCompatibility {
            protocol_version: 2,
            mandatory_features: vec![1, 2],
        });
        open.git_acceptance = Some(GitPushAcceptance {
            expected_revision: scope.expected_native.as_bytes().to_vec(),
            expected_git_commit: old.as_bytes().to_vec(),
            accepted_git_commit: new.as_bytes().to_vec(),
            gateway_publisher: scope.publisher.to_vec(),
            transport_token: actor.transport_token.into(),
            history: histories,
            scope: Some(actor.scope.clone()),
            expected_native_generation: Some(expected_native_generation),
            originals_digest: originals_digest.to_vec(),
        });
        remote
            .publish_git_content(tip, uploads, actor.account_id)
            .await
            .map_err(failure)
    }
}
