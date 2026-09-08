//! Prepare creation once and retain the original signed identity for retries
//! and later replication. Preparation performs no network or repository I/O.
use api::v2::client::{ClientError, RpcTransport};
use crypto::Signer;
use heddle_object_model::object::{OperationId, thread_replication::ThreadGenesis};

use crate::{Remote, contract::*, replication::opening, rpc, transport::Error};

pub struct ThreadCreation {
    request: StartThreadRequest,
    reference: ThreadRef,
}

impl ThreadCreation {
    pub fn sign(
        operation_id: impl Into<String>,
        genesis: &ThreadGenesis,
        signer: &impl Signer,
    ) -> Result<Self, Error> {
        let operation_id = operation_id.into();
        validate_operation(&operation_id)?;
        Self::from_signed(operation_id, opening::sign_genesis(genesis, signer)?)
    }

    /// Relay an existing creator record without replacing its author or key.
    pub fn from_signed(
        operation_id: impl Into<String>,
        record: SignedRecord,
    ) -> Result<Self, Error> {
        let operation_id = operation_id.into();
        validate_operation(&operation_id)?;
        let genesis = ThreadGenesis::decode(&record.canonical_record)
            .map_err(|_| Error::Protocol("invalid canonical Thread genesis"))?;
        let reference = ThreadRef {
            spool: Some(SpoolRef {
                id: genesis.spool.clone(),
            }),
            id: Some(ThreadId {
                value: genesis
                    .id()
                    .map_err(|_| Error::Protocol("invalid Thread identity"))?
                    .as_bytes()
                    .to_vec(),
            }),
        };
        opening::verify_genesis(&record, &reference)?;
        Ok(Self {
            request: StartThreadRequest {
                client_operation_id: operation_id,
                spool: reference.spool.clone(),
                thread_genesis: Some(record),
            },
            reference,
        })
    }

    pub fn request(&self) -> &StartThreadRequest {
        &self.request
    }
    pub fn reference(&self) -> &ThreadRef {
        &self.reference
    }
}

fn validate_operation(operation_id: &str) -> Result<(), Error> {
    operation_id
        .parse::<OperationId>()
        .map(|_| ())
        .map_err(|_| Error::Protocol("client operation ID must be a UUID"))
}

impl<T: RpcTransport<Error = Error>> Remote<T> {
    /// One command returns its receipt and resulting overview. Bind subsequent
    /// calls with `remote.thread(creation.reference().clone())`, without lookup.
    pub async fn start_thread(
        &self,
        creation: &ThreadCreation,
    ) -> Result<ThreadMutationResponse, ClientError<Error>> {
        if !self
            .description
            .understood_signed_record_formats
            .iter()
            .any(|format| format == heddle_object_model::object::thread_replication::GENESIS_FORMAT)
        {
            return Err(ClientError::Transport(Error::Protocol(
                "endpoint does not understand Thread genesis",
            )));
        }
        let response = self
            .api
            .call::<rpc::ThreadServiceStartThread>(creation.request())
            .await?;
        let receipt = response.receipt.as_ref().ok_or_else(|| {
            ClientError::Transport(Error::Protocol("Thread creation response has no receipt"))
        })?;
        if receipt.client_operation_id != creation.request.client_operation_id
            || receipt.endpoint != self.description.endpoint
            || receipt.outcome.is_none()
            || response
                .thread
                .as_ref()
                .is_some_and(|thread| thread.r#ref.as_ref() != Some(creation.reference()))
            || (matches!(receipt.outcome, Some(mutation_receipt::Outcome::Applied(_)))
                && response.thread.is_none())
        {
            return Err(ClientError::Transport(Error::Protocol(
                "Thread creation response does not match the command",
            )));
        }
        Ok(response)
    }
}
