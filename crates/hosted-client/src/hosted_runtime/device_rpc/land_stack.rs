//! Ordered logical landings commit as one local Spool metadata transaction.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1::*;
use crypto::{Signer, thread_operation::SignedOperation};
use objects::{
    object::{ContentHash, State, VisibilityTier, thread_replication::{ThreadFacet, ThreadOperation, ThreadOperationBody, local_integration::LocalIntegration}},
    store::ObjectStore,
};
use prost::Message;
use repo::thread_replication::ThreadReplica;

use super::{DeviceRpc, auth::{Session, source_visibility_floor}, checkout::{revision, thread}};

struct TargetCursor {
    state: objects::object::StateId,
    frontier: BTreeSet<ContentHash>,
    floor: VisibilityTier,
    prepared: Vec<ThreadOperation>,
}

impl DeviceRpc {
    pub(super) fn land_stack(&self, session: &Session, request: LandStackRequest) -> Result<Vec<u8>> {
        let Some(spool) = request.spool.as_ref() else { bail!("landing stack requires a Spool") };
        if spool.id != session.spool.id.to_string() { bail!("landing stack Spool does not match authority") }
        if request.landings.is_empty() || request.landings.len() > 64 { bail!("landing stack requires 1–64 members") }
        let repository = repo::Repository::open(&session.spool.root)?;
        let operation_id: objects::object::OperationId = request.client_operation_id.parse()?;
        let namespace = format!("device-land-stack/{}/{}", session.principal, session.actor);
        let request_hash = *blake3::hash(&request.encode_to_vec()).as_bytes();
        let command = repo::device_operations::Command {
            namespace: &namespace,
            id: operation_id,
            method: "/heddle.api.v2alpha1.ThreadService/LandStack",
            request_hash,
        };
        session.check_current(&self.home)?;
        if let Some(response) = repo::device_operations::replay_response(&session.spool.heddle_dir, &command)? {
            return Ok(response);
        }
        let mut sources = BTreeSet::new();
        let mut targets = BTreeSet::new();
        for landing in &request.landings {
            sources.insert(thread(session, landing.thread.as_ref())?);
            targets.insert(thread(session, landing.target.as_ref())?);
        }
        if !sources.is_disjoint(&targets) { bail!("landing stack source and target Threads must be disjoint") }
        let anchor = *targets.iter().next().context("landing stack target missing")?;
        let anchor_replica = ThreadReplica::open(&session.spool.heddle_dir, anchor)?;
        let prepared = anchor_replica.prepared_local_stack(&namespace, &request.client_operation_id, &request_hash)?;
        let principal = uuid::Uuid::parse_str(&session.principal)?;
        let (operations, response) = if let Some((canonical, response)) = prepared {
            let items: Vec<Vec<u8>> = rmp_serde::from_slice(&canonical)?;
            if items.len() != request.landings.len() { bail!("prepared stack length does not match request") }
            let mut operations = Vec::with_capacity(items.len());
            for (encoded, landing) in items.iter().zip(&request.landings) {
                let operation = ThreadOperation::decode(encoded)?;
                let integration = operation.local_integration()?.context("prepared stack member is not a local integration")?;
                if operation.thread != thread(session, landing.target.as_ref())?
                    || integration.source_thread != thread(session, landing.thread.as_ref())?
                    || integration.source_revision != revision(session, landing.source.as_ref())?
                    || integration.target_thread != operation.thread
                    || integration.local_policy_version.as_bytes().as_slice() != landing.expected_policy_version
                { bail!("prepared stack member does not match request") }
                operations.push(operation);
            }
            (operations, response)
        } else {
            let policy = super::land::thread_policy_version(&repository)?;
            let mut cursors: BTreeMap<ContentHash, TargetCursor> = BTreeMap::new();
            let mut initial_expected: BTreeMap<ContentHash, objects::object::StateId> = BTreeMap::new();
            let mut operations = Vec::with_capacity(request.landings.len());
            for landing in &request.landings {
                let source_id = thread(session, landing.thread.as_ref())?;
                let source_state = revision(session, landing.source.as_ref())?;
                let target_id = thread(session, landing.target.as_ref())?;
                let expected = revision(session, landing.expected_target.as_ref())?;
                if landing.expected_policy_version != policy.as_bytes() { bail!("landing policy changed; observe landing again") }
                let source = ThreadReplica::open(&session.spool.heddle_dir, source_id)?;
                let target = ThreadReplica::open(&session.spool.heddle_dir, target_id)?;
                session.authorize_thread(&repository, &source)?;
                session.authorize_thread(&repository, &target)?;
                let source_floor = source_visibility_floor(&repository, &source, principal, session.agent_id.as_deref(), source_state)?
                    .context("source revision is unavailable to this caller")?;
                let cursor = if let Some(cursor) = cursors.get_mut(&target_id) {
                    if initial_expected.get(&target_id) != Some(&expected) { bail!("stack target comparison changed within request") }
                    cursor
                } else {
                    let target_floor = source_visibility_floor(&repository, &target, principal, session.agent_id.as_deref(), expected)?
                        .context("target revision is unavailable to this caller")?;
                    let view = target.view()?;
                    let heads = if view.source_heads.is_empty() { BTreeSet::from([target.genesis()?.base]) } else { view.source_heads };
                    if heads != BTreeSet::from([expected]) { bail!("target changed or has unresolved source heads; observe landing again") }
                    initial_expected.insert(target_id, expected);
                    let frontier = view.frontiers.get(&ThreadFacet::Source).cloned().unwrap_or_default();
                    cursors.entry(target_id).or_insert(TargetCursor { state: expected, frontier, floor: target_floor, prepared: Vec::new() })
                };
                let source_originals = source.source_operation_page(source_state, None, 2)?;
                let [source_operation] = source_originals.as_slice() else { bail!("stack source must have one exact admitted original") };
                let tree = match verbs::merge::try_three_way_merge_between_tips(&repository, &cursor.state, &source_state, verbs::merge::ConflictLabels::DEFAULT)? {
                    verbs::merge::ThreeWayMergeOutcome::Clean { tree } => repository.store().put_tree(&tree)?,
                    verbs::merge::ThreeWayMergeOutcome::FastForward { target } => repository.store().get_state(&target)?.context("source state missing")?.tree,
                    verbs::merge::ThreeWayMergeOutcome::AlreadyIntegrated { .. } => repository.store().get_state(&cursor.state)?.context("target state missing")?.tree,
                    verbs::merge::ThreeWayMergeOutcome::Conflicted { paths, .. } => bail!("landing has unresolved file conflicts: {}", paths.join(", ")),
                };
                let floor = objects::object::thread_replication::local_integration::intersect_visibility(&cursor.floor, &source_floor)?;
                let floor = objects::object::thread_replication::local_integration::intersect_visibility(&repository.resolve_capture_default_visibility(), &floor)?;
                let state = State::new_merge(tree, vec![cursor.state, source_state], session.attribution.clone()).with_intent("Device Thread stack landing");
                repository.store().put_state(&state)?;
                let signer = repository.native_thread_signer(&target)?;
                let device: [u8;32] = signer.public_key().try_into().map_err(|_| anyhow::anyhow!("invalid publisher key"))?;
                let integration = LocalIntegration {
                    author: target.source_author_for(&device)?, version: 1, spool: session.spool.id, device,
                    source_thread: source_id, source_operation: *source_operation, source_revision: source_state,
                    target_thread: target_id, expected_target_frontier: cursor.frontier.clone(),
                    result: target.prepare_integration_with_prepared(&repository, &state, source_id, *source_operation, &cursor.prepared)?,
                    result_visibility: floor.clone(), initiating_request_proof: session.request_proof,
                    local_policy_version: policy, executed_at_ms: chrono::Utc::now().timestamp_millis(),
                };
                let operation = ThreadOperation {
                    version: 1, thread: target_id, parents: cursor.frontier.clone(), publisher: device,
                    body: ThreadOperationBody::LocalIntegration(integration.encode()?),
                };
                cursor.frontier = BTreeSet::from([operation.id()?]);
                cursor.state = state.id();
                cursor.floor = floor;
                cursor.prepared.push(operation.clone());
                operations.push(operation);
            }
            let response = MutationResponse { receipt: Some(self.receipt(&request.client_operation_id)) }.encode_to_vec();
            let canonical = rmp_serde::to_vec(&operations.iter().map(ThreadOperation::encode).collect::<Result<Vec<_>,_>>()?)?;
            let (canonical, response) = anchor_replica.prepare_local_stack(&namespace, &request.client_operation_id, &request_hash, &canonical, &response)?;
            let bytes: Vec<Vec<u8>> = rmp_serde::from_slice(&canonical)?;
            let operations = bytes.iter().map(|bytes| ThreadOperation::decode(bytes)).collect::<Result<Vec<_>,_>>()?;
            (operations, response)
        };
        let mut signed = Vec::with_capacity(operations.len());
        for operation in &operations {
            let replica = ThreadReplica::open(&session.spool.heddle_dir, operation.thread)?;
            let signer = repository.native_thread_signer(&replica)?;
            signed.push(SignedOperation::sign(operation, &signer)?);
        }
        session.check_current(&self.home)?;
        anchor_replica.receive_local_stack_cas_command(&signed, repository.store(), &command, &response)?;
        Ok(response)
    }
}
