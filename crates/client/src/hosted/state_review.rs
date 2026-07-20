// SPDX-License-Identifier: Apache-2.0
//! Hosted `StateReviewService` client methods.
//!
//! Review signatures are server-minted via caller-authenticated, PoP-signed
//! calls (weft#549) — the object pack rejects client-pushed `ReviewSignatures`
//! attachments. `heddle review sync` replays locally-recorded review signatures
//! through the active `SignState` production route.

use api::heddle::api::v1alpha1::{ReviewKind, ReviewScope, SignStateRequest, SignStateResponse};
use objects::object::StateId;
use wire::ProtocolError;

use super::{HostedClient, helpers::hosted_to_protocol_error, operation_id::ClientOperationId};

const SIGN_STATE: &str = "heddle.api.v1alpha1.StateReviewService/SignState";

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
}
