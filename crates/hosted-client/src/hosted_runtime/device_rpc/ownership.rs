//! An explicit owned-device command may co-sign only a fully bound Thread claim,
//! after verifying the account acceptor and the current original local owner.
use anyhow::{Context, Result, ensure};
use prost::Message;
use api::heddle::api::v2alpha1::*;
use objects::object::{OperationId, thread_replication::{ThreadFacet, ownership_claim::METHOD}};
use repo::thread_replication::{ThreadReplica, ownership_claim::verify_claim_account_authority};
use thread_api::thread_ownership::{self, ClaimProof};
use super::{DeviceRpc, auth::Session, checkout};

impl DeviceRpc {
    pub(super) fn claim_thread_ownership(&self, session: &Session, body: &[u8]) -> Result<Vec<u8>> {
        let request = ClaimThreadOwnershipRequest::decode(body)?;
        let operation: OperationId = request.client_operation_id.parse()?;
        let thread = checkout::thread(session, request.thread.as_ref())?;
        let repository = repo::Repository::open(&session.spool.root)?;
        let replica = ThreadReplica::open(&session.spool.heddle_dir, thread)?;
        session.authorize_thread(&repository, &replica)?;
        let namespace = session.command_namespace()?;
        let command = repo::device_operations::Command { namespace: &namespace, id: operation,
            method: METHOD, request_hash: *blake3::hash(body).as_bytes(),
        };
        if let Some(response) = repo::device_operations::replay_response(&session.spool.heddle_dir, &command)? { return Ok(response); }
        let proof = thread_ownership::decode(request.claim.as_ref().context("claim required")?)?;
        let accepting_account = match &proof {
            ClaimProof::Complete(value) => value.verify()?.account()?,
            ClaimProof::Acceptance(value) => value.verify()?.account()?,
        };
        ensure!(accepting_account == uuid::Uuid::parse_str(&session.principal)?, "claim acceptance belongs to another authenticated account");
        let now = chrono::Utc::now().timestamp();
        let authority = repo::device_authority::load(&self.home, now)?;
        let complete = match proof {
            ClaimProof::Complete(proof) => proof,
            ClaimProof::Acceptance(acceptance) => {
                let statement = acceptance.verify()?;
                statement.validate_genesis(&replica.genesis()?)?;
                if let Some(retained) = replica.ownership_claims()?.into_iter().find(|proof| proof.canonical == acceptance.canonical && proof.acceptance_signature == acceptance.signature) {
                    retained
                } else {
                    verify_claim_account_authority(&statement, &replica.genesis()?, &authority, &session.spool.capability_path, now)?;
                    let frontier = replica.frontier_page(ThreadFacet::Source, None, 129)?.into_iter().collect();
                    ensure!(statement.source_frontier == frontier, "ownership claim source frontier changed");
                    ensure!(replica.effective_owner()? == objects::object::thread_replication::GenesisOwner::LocalKey(statement.prior_local_key),
                        "Thread already has an account owner; use its retained ownership proof");
                    let local = repository.native_thread_signer(&replica)?;
                    acceptance.cosign(&local)?
                }
            }
        };
        let statement = complete.verify()?;
        let claim_id = statement.id()?;
        let response = ClaimThreadOwnershipResponse {
            receipt: Some(self.receipt(&request.client_operation_id)), thread: request.thread,
            owner: Some(PrincipalRef { id: statement.account()?.to_string() }),
            claim_id: claim_id.as_bytes().to_vec(), claim: Some(thread_ownership::encode(&complete)?),
        }.encode_to_vec();
        session.check_current(&self.home)?;
        session.authorize_thread(&repository, &replica)?;
        Ok(replica.claim_ownership_with_command(&complete, &authority, &session.spool.capability_path, now, &command, &response)?)
    }
}
