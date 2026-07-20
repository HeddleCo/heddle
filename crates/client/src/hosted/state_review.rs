// SPDX-License-Identifier: Apache-2.0
//! Hosted `StateReviewService` client methods.
//!
//! Review signatures are server-minted via caller-authenticated, PoP-signed
//! RPCs (weft#549) — the object pack rejects client-pushed `ReviewSignatures`
//! attachments, so `SignState`/`RecordVerdict` are the ONLY channel by which a
//! signature reaches the hosted server. `heddle review sync` (push path) replays
//! locally-recorded review signatures over these methods.

use api::heddle::api::v1alpha1::{
    ListSignaturesRequest, ListSignaturesResponse, RecordVerdictRequest, RecordVerdictResponse,
    ReviewKind, ReviewScope, SignStateRequest, SignStateResponse, Verdict,
};
use objects::object::StateId;
use wire::ProtocolError;

use super::{HostedClient, helpers::hosted_to_protocol_error, operation_id::ClientOperationId};

const SIGN_STATE: &str = "heddle.api.v1alpha1.StateReviewService/SignState";
const RECORD_VERDICT: &str = "heddle.api.v1alpha1.StateReviewService/RecordVerdict";

impl HostedClient {
    /// Mint a hosted review signature over `state_id`. `signature`/`public_key`
    /// are the raw bytes the caller already computed over the canonical
    /// `signing_payload` (byte-identical to the server's reconstruction), so the
    /// server verifies the exact same signature the local `review sign` wrote.
    #[allow(clippy::too_many_arguments)]
    pub async fn sign_state(
        &mut self,
        repo_path: &str,
        state_id: &StateId,
        kind: ReviewKind,
        scope: ReviewScope,
        justification: &str,
        algorithm: &str,
        public_key: Vec<u8>,
        signature: Vec<u8>,
        signed_at_unix: i64,
        client_operation_id: String,
    ) -> Result<SignStateResponse, ProtocolError> {
        let operation_id = ClientOperationId::for_required_method(SIGN_STATE, client_operation_id)?;
        let request = SignStateRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            state_id: super::helpers::proto_state_id(*state_id),
            kind: kind as i32,
            scope: Some(scope),
            justification: justification.to_string(),
            algorithm: algorithm.to_string(),
            public_key,
            signature,
            signed_at: Some(prost_types::Timestamp {
                seconds: signed_at_unix,
                nanos: 0,
            }),
            client_operation_id: operation_id.to_wire(),
        };
        self.routes()
            .sign_state(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }

    /// Record a signed SIGN/HOLD/REJECT reviewer verdict over `state_id`.
    /// Decoupled from status — returns the same state. Same signing model as
    /// [`Self::sign_state`].
    #[allow(clippy::too_many_arguments)]
    pub async fn record_verdict(
        &mut self,
        repo_path: &str,
        state_id: &StateId,
        verdict: Verdict,
        scope: ReviewScope,
        reason: &str,
        algorithm: &str,
        public_key: Vec<u8>,
        signature: Vec<u8>,
        signed_at_unix: i64,
        client_operation_id: String,
    ) -> Result<RecordVerdictResponse, ProtocolError> {
        let operation_id =
            ClientOperationId::for_required_method(RECORD_VERDICT, client_operation_id)?;
        let request = RecordVerdictRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            state_id: super::helpers::proto_state_id(*state_id),
            verdict: verdict as i32,
            scope: Some(scope),
            reason: reason.to_string(),
            algorithm: algorithm.to_string(),
            public_key,
            signature,
            signed_at: Some(prost_types::Timestamp {
                seconds: signed_at_unix,
                nanos: 0,
            }),
            client_operation_id: operation_id.to_wire(),
        };
        self.routes()
            .record_verdict(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }

    /// List hosted review signatures recorded against `state_id`. Read-only,
    /// authenticated (unsigned tier).
    pub async fn list_review_signatures(
        &mut self,
        repo_path: &str,
        state_id: &StateId,
    ) -> Result<ListSignaturesResponse, ProtocolError> {
        let request = ListSignaturesRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            state_id: super::helpers::proto_state_id(*state_id),
        };
        self.routes()
            .list_signatures(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }
}
