//! Device landing creates ordinary signed source work in the target Thread.
//! The target replica is authoritative; other checkouts refresh independently.
use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1::*;
use prost::Message;
use crypto::{Signer, thread_operation::SignedOperation};
use objects::{
    object::{
        ContentHash, State,
        thread_replication::{
            Admission, ThreadFacet, ThreadOperation, ThreadOperationBody,
            local_integration::LocalIntegration,
        },
    },
    store::ObjectStore,
};
use repo::thread_replication::ThreadReplica;

use super::{
    DeviceRpc,
    auth::{Session, source_content_visibility},
    checkout::{decode_token, revision, thread, verified_lease},
    read_bounded,
};

pub(super) fn policy_version(repository: &repo::Repository) -> Result<ContentHash> {
    Ok(ContentHash::compute_typed("heddle-device-integration-policy-v2", &[b"same-spool;root-derived-source-and-target-authority;writer-lease;explicit-source;all-target-parents;clean-checkout;conflict-free-three-way;target-frontier-cas;preserve-audience;no-hosted-approval".as_slice(),serde_json::to_vec(&repository.resolve_capture_default_visibility())?.as_slice()].concat()))
}
pub(super) fn thread_policy_version(repository: &repo::Repository) -> Result<ContentHash> {
    Ok(ContentHash::compute_typed("heddle-device-thread-landing-policy-v2", &[b"same-spool;independently-authorized-source-and-target;explicit-source;unique-target-head;conflict-free-three-way;target-frontier-cas;preserve-audience;no-checkout-write;no-hosted-approval".as_slice(),serde_json::to_vec(&repository.resolve_capture_default_visibility())?.as_slice()].concat()))
}
impl DeviceRpc {
    pub(super) fn land_thread(
        &self,
        session: &Session,
        request: LandThreadRequest,
    ) -> Result<Vec<u8>> {
        let repository = repo::Repository::open(&session.spool.root)?;
        let source_thread = thread(session, request.thread.as_ref())?;
        let source = revision(session, request.source.as_ref())?;
        let target = thread(session, request.target.as_ref())?;
        let expected = revision(session, request.expected_target.as_ref())?;
        if source_thread == target {
            bail!("landing requires distinct source and target Threads");
        }
        let operation_id: objects::object::OperationId = request.client_operation_id.parse()?;
        let namespace = format!("device-land/{}/{}", session.principal, session.actor);
        let request_hash = *blake3::hash(&request.encode_to_vec()).as_bytes();
        let command = repo::device_operations::Command {
            namespace: &namespace,
            id: operation_id,
            method: "/heddle.api.v2alpha1.ThreadService/LandThread",
            request_hash,
        };
        session.check_current(&self.home)?;
        if let Some(response) = repo::device_operations::replay_response(
            &session.spool.heddle_dir,
            &command,
        )? {
            return Ok(response);
        }
        if request.expected_policy_version != thread_policy_version(&repository)?.as_bytes() {
            bail!("local integration policy changed; observe landing again");
        }
        let source_replica = ThreadReplica::open(&session.spool.heddle_dir, source_thread)?;
        let target_replica = ThreadReplica::open(&session.spool.heddle_dir, target)?;
        session.authorize_thread(&repository, &source_replica)?;
        session.authorize_thread(&repository, &target_replica)?;
        if source_replica.view()?.source_heads != BTreeSet::from([source]) {
            bail!("landing source must be the single current Thread head");
        }
        let principal = uuid::Uuid::parse_str(&session.principal)?;
        if source_content_visibility(
            &repository,
            &source_replica,
            principal,
            session.agent_id.as_deref(),
            source,
        )?
        .is_none()
        {
            bail!("source revision is unavailable to this caller");
        }
        if source_content_visibility(
            &repository,
            &target_replica,
            principal,
            session.agent_id.as_deref(),
            expected,
        )?
        .is_none()
        {
            bail!("target revision is unavailable to this caller");
        }
        let prepared = target_replica.prepared_local_landing(
            &namespace,
            &request.client_operation_id,
            &request_hash,
        )?;
        let signer = repository.native_thread_signer(&target_replica)?;
        let (operation, response) = if let Some((canonical, response)) = prepared {
            let operation = ThreadOperation::decode(&canonical)?;
            let integration = operation
                .local_integration()?
                .context("prepared landing operation is not an integration")?;
            if integration.source_thread != source_thread
                || integration.source_revision != source
                || integration.target_thread != target
                || integration.local_policy_version != thread_policy_version(&repository)?
                || operation.thread != target
            {
                bail!("prepared landing does not match this request");
            }
            (operation, response)
        } else {
            let originals = source_replica.source_operation_page(source, None, 2)?;
            let [source_operation] = originals.as_slice() else {
                bail!("landing source must have one exact admitted original");
            };
            let view = target_replica.view()?;
            let heads = if view.source_heads.is_empty() {
                BTreeSet::from([target_replica.genesis()?.base])
            } else {
                view.source_heads
            };
            if heads != BTreeSet::from([expected]) {
                bail!("target changed or has unresolved source heads; observe landing again");
            }
            use verbs::merge::{
                ConflictLabels, ThreeWayMergeOutcome, try_three_way_merge_between_tips,
            };
            let tree = match try_three_way_merge_between_tips(
                &repository,
                &expected,
                &source,
                ConflictLabels::DEFAULT,
            )? {
                ThreeWayMergeOutcome::Clean { tree } => repository.store().put_tree(&tree)?,
                ThreeWayMergeOutcome::FastForward { target } => repository
                    .store()
                    .get_state(&target)?
                    .context("source state missing")?
                    .tree,
                ThreeWayMergeOutcome::AlreadyIntegrated { .. } => repository
                    .store()
                    .get_state(&expected)?
                    .context("target state missing")?
                    .tree,
                ThreeWayMergeOutcome::Conflicted { paths, .. } => {
                    bail!("landing has unresolved file conflicts: {}", paths.join(", "))
                }
            };
            let mut result_visibility = repository.resolve_capture_default_visibility();
            for parent in [expected, source] {
                result_visibility =
                    objects::object::thread_replication::local_integration::intersect_visibility(
                        &result_visibility,
                        &repository.effective_visibility_tier(&parent)?,
                    )?;
            }
            let state = State::new_merge(
                tree,
                vec![expected, source],
                session.attribution.clone(),
            )
            .with_intent("Device Thread landing");
            let frontier = view
                .frontiers
                .get(&ThreadFacet::Source)
                .cloned()
                .unwrap_or_default();
            let device = signer
                .public_key()
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid publisher key"))?;
            let integration = LocalIntegration {
                author: target_replica.source_author_for(&device)?,
                version: 1,
                spool: session.spool.id,
                device,
                source_thread,
                source_operation: *source_operation,
                source_revision: source,
                target_thread: target,
                expected_target_frontier: frontier.clone(),
                result: target_replica.prepare_integration(
                    &repository,
                    &state,
                    source_thread,
                    *source_operation,
                )?,
                result_visibility,
                initiating_request_proof: session.request_proof,
                local_policy_version: thread_policy_version(&repository)?,
                executed_at_ms: chrono::Utc::now().timestamp_millis(),
            };
            let operation = ThreadOperation {
                version: 1,
                thread: target,
                parents: frontier,
                publisher: device,
                body: ThreadOperationBody::LocalIntegration(integration.encode()?),
            };
            let response = ThreadMutationResponse {
                receipt: Some(self.receipt(&request.client_operation_id)),
                thread: None,
            }
            .encode_to_vec();
            let (canonical, response) = target_replica.prepare_local_landing(
                &namespace,
                &request.client_operation_id,
                &request_hash,
                &operation.encode()?,
                &response,
            )?;
            (ThreadOperation::decode(&canonical)?, response)
        };
        let signed = SignedOperation::sign(&operation, &signer)?;
        session.check_current(&self.home)?;
        match target_replica.receive_local_integration_cas_command(
            &signed,
            repository.store(),
            |_| Ok(()),
            &command,
            &response,
        )? {
            Admission::Accepted => {},
            other => bail!("landing was not admitted: {other:?}"),
        }
        Ok(response)
    }

    pub(super) fn land_checkout(
        &self,
        session: &Session,
        request: LandCheckoutRequest,
    ) -> Result<Vec<u8>> {
        let checkout = self.checked_checkout(session, request.checkout.as_ref())?;
        let token = decode_token(&request.writer_lease_token)?;
        verified_lease(&checkout, session, &token)?;
        let _writer = checkout.repository.authenticate_checkout_writer(
            checkout.binding.thread,
            &token.id,
            &token.secret,
        )?;
        let source = revision(session, request.source.as_ref())?;
        let target = thread(session, request.target.as_ref())?;
        let expected = revision(session, request.expected_target.as_ref())?;
        if target == checkout.binding.thread {
            bail!("landing needs a distinct target Thread");
        }
        if request.expected_policy_version != policy_version(&checkout.repository)?.as_bytes() {
            bail!("local integration policy changed");
        }
        let target_replica = ThreadReplica::open(&session.spool.heddle_dir, target)?;
        let path = session
            .spool
            .heddle_dir
            .join("device-checkout-commands")
            .join(format!("{}.landing", request.client_operation_id));
        let signer = checkout.repository.native_thread_signer(&target_replica)?;
        let operation = if path.exists() {
            let operation = ThreadOperation::decode(&read_bounded(&path, 256 * 1024)?)?;
            let receipt = operation
                .local_integration()?
                .context("landing journal type changed")?;
            if receipt.source_thread != checkout.binding.thread
                || receipt.source_revision != source
                || receipt.target_thread != target
                || receipt.local_policy_version != policy_version(&checkout.repository)?
            {
                bail!("landing journal intent changed");
            }
            operation
        } else {
            self.check_version(session, &checkout, &request.expected_checkout_version)?;
            if checkout.repository.head()? != Some(source)
                || !checkout.repository.worktree_matches_state(&source)?
            {
                bail!("landing requires the observed clean source checkout");
            }
            let source_replica =
                ThreadReplica::open(&session.spool.heddle_dir, checkout.binding.thread)?;
            let source_operation = *source_replica
                .source_operation_page(source, None, 1)?
                .first()
                .context("source has no admitted original operation")?;
            let view = target_replica.view()?;
            let target_heads = if view.source_heads.is_empty() {
                BTreeSet::from([target_replica.genesis()?.base])
            } else {
                view.source_heads
            };
            if target_heads != BTreeSet::from([expected]) {
                bail!("target changed or has unresolved source heads");
            }
            use verbs::merge::{
                ConflictLabels, ThreeWayMergeOutcome, try_three_way_merge_between_tips,
            };
            let tree = match try_three_way_merge_between_tips(
                &checkout.repository,
                &expected,
                &source,
                ConflictLabels::DEFAULT,
            )? {
                ThreeWayMergeOutcome::Clean { tree } => {
                    checkout.repository.store().put_tree(&tree)?
                }
                ThreeWayMergeOutcome::FastForward { target } => {
                    checkout
                        .repository
                        .store()
                        .get_state(&target)?
                        .context("merge target missing")?
                        .tree
                }
                ThreeWayMergeOutcome::AlreadyIntegrated { .. } => {
                    checkout
                        .repository
                        .store()
                        .get_state(&expected)?
                        .context("target missing")?
                        .tree
                }
                ThreeWayMergeOutcome::Conflicted { paths, .. } => bail!(
                    "landing has unresolved file conflicts: {}",
                    paths.join(", ")
                ),
            };
            let parents = BTreeSet::from([expected, source]);
            let mut result_visibility = checkout.repository.resolve_capture_default_visibility();
            for parent in &parents {
                result_visibility =
                    objects::object::thread_replication::local_integration::intersect_visibility(
                        &result_visibility,
                        &checkout.repository.effective_visibility_tier(parent)?,
                    )?;
            }

            let state = State::new_merge(
                tree,
                parents.into_iter().collect(),
                session.attribution.clone(),
            )
            .with_intent("Device Thread landing");
            let frontier = view
                .frontiers
                .get(&ThreadFacet::Source)
                .cloned()
                .unwrap_or_default();
            let receipt = LocalIntegration {
                author: target_replica.source_author_for(
                    &signer
                        .public_key()
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("invalid publisher key"))?,
                )?,
                version: 1,
                spool: session.spool.id,
                device: signer
                    .public_key()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("invalid publisher key"))?,
                source_thread: checkout.binding.thread,
                source_operation,
                source_revision: source,
                target_thread: target,
                expected_target_frontier: frontier.clone(),
                result: target_replica.prepare_integration(
                    &checkout.repository,
                    &state,
                    checkout.binding.thread,
                    source_operation,
                )?,
                result_visibility,
                initiating_request_proof: session.request_proof,
                local_policy_version: policy_version(&checkout.repository)?,
                executed_at_ms: chrono::Utc::now().timestamp_millis(),
            };
            let operation = ThreadOperation {
                version: 1,
                thread: target,
                parents: frontier,
                publisher: receipt.device,
                body: ThreadOperationBody::LocalIntegration(receipt.encode()?),
            };
            objects::fs_atomic::write_file_atomic_secret(&path, &operation.encode()?)?;
            operation
        };
        let signed = SignedOperation::sign(&operation, &signer)?;
        session.check_current(&self.home)?;
        if target_replica
            .receive_prepared_source(&signed, checkout.repository.store(), |_| Ok(()))?
            != Admission::Accepted
        {
            bail!("local integration was not admitted");
        }
        self.checkout_response(session, &checkout, &request.client_operation_id, None)
    }
}
