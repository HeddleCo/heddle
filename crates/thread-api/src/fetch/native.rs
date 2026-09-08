//! Install a verified hosted download through the existing local store and
//! replica admission paths. This disk operation never advances a checkout.
use crypto::thread_operation::SignedGenesis;
use heddle_object_model::object::{
    ContentHash, StateId,
    thread_replication::integration::{SPOOL_GENESIS_TRUST_FORMAT, TrustedHostedExecutor},
};
use objects::store::ObjectStore;
use prost::Message;
use repo::{Repository, thread_replication::ThreadReplica};

use super::{Error, StagedSource};
use crate::contract::EndpointKind;

impl StagedSource {
    /// Call on the application's disk worker. Owner history is independently
    /// verified and monotonically pinned; the authenticated hosted endpoint is
    /// pinned before accepting any original Integration operations.
    pub fn install(self, repository: &Repository, now_unix_seconds: i64) -> Result<StateId, Error> {
        let endpoint = self
            .ready
            .endpoint
            .as_ref()
            .ok_or(Error::Invalid("endpoint absent"))?;
        if endpoint.kind != EndpointKind::Weft as i32 {
            return Err(Error::Invalid(
                "hosted installation requires the selected Weft endpoint",
            ));
        }
        let executor = endpoint
            .public_key
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invalid("invalid hosted endpoint key"))?;
        let spool = uuid::Uuid::parse_str(
            &self
                .ready
                .thread
                .as_ref()
                .and_then(|t| t.spool.as_ref())
                .ok_or(Error::Invalid("Spool absent"))?
                .id,
        )
        .map_err(preparation)?;
        let genesis = self
            .ready
            .owner_genesis
            .as_ref()
            .ok_or(Error::Invalid("owner genesis absent"))?;
        let owner = self
            .ready
            .ownership
            .as_ref()
            .ok_or(Error::Invalid("owner history absent"))?;
        let verified =
            repo::verify_spool_owner_observation(genesis, owner, spool, now_unix_seconds)
                .map_err(preparation)?;
        let body = genesis
            .genesis
            .as_ref()
            .ok_or(Error::Invalid("owner genesis body absent"))?;
        let trust = TrustedHostedExecutor {
            spool,
            spool_genesis: ContentHash::compute_typed(
                SPOOL_GENESIS_TRUST_FORMAT,
                &body.encode_to_vec(),
            ),
            executor,
        };
        for signed in &self.operations {
            let operation = signed.verify().map_err(preparation)?;
            if operation.integration().map_err(preparation)?.is_some() {
                trust.authorize(&operation).map_err(preparation)?;
            }
        }
        let original = self
            .ready
            .thread_genesis
            .as_ref()
            .ok_or(Error::Invalid("Thread genesis absent"))?;
        let [signature] = original.signatures.as_slice() else {
            return Err(Error::Invalid("one original creator signature required"));
        };
        let signed = SignedGenesis {
            canonical: original.canonical_record.clone(),
            signature: signature.signature.clone(),
        };
        repository
            .verify_and_pin_owner_observation(
                genesis,
                owner,
                spool,
                &verified.wire().canonical_spool_path_segments,
                now_unix_seconds,
            )
            .map_err(preparation)?;
        let replica =
            ThreadReplica::create(repository.heddle_dir(), &signed).map_err(preparation)?;
        repository
            .pin_thread_hosted_executor(&replica, executor)
            .map_err(preparation)?;
        repository
            .store()
            .install_pack_streaming(
                &self.directory.path().join("source.pack"),
                &self.directory.path().join("source.idx"),
            )
            .map_err(preparation)?;
        // The exact selected read authorized this material; original signatures
        // and causal rules still govern durable admission. Metadata can arrive
        // child-first; the replica settles it when its original parents arrive.
        for operation in &self.operations {
            replica
                .receive(operation, repository.store(), |_| Ok(()))
                .map_err(preparation)?;
        }
        if replica
            .accepted_source_revision(self.state.id())
            .map_err(preparation)?
            .is_none()
        {
            return Err(Error::Invalid("selected source proof did not settle"));
        }
        Ok(self.state.id())
    }
}
fn preparation(error: impl std::fmt::Display) -> Error {
    Error::Preparation(error.to_string())
}
