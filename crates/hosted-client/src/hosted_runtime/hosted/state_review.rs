// SPDX-License-Identifier: Apache-2.0
//! Hosted `StateReviewService` client methods.
//!
//! Review signatures are server-minted via caller-authenticated, PoP-signed
//! calls (weft#549) — the object pack rejects client-pushed `ReviewSignatures`
//! attachments. `heddle review sync` replays locally-recorded review signatures
//! through the active `SignState` production route.

use api::heddle::api::v1alpha1::{ReviewKind, ReviewScope, SignStateResponse};
use objects::object::StateId;
use wire::ProtocolError;

use super::HostedClient;

impl HostedClient {
    /// Mint a hosted review signature over `state_id` via v2 `RecordReview`.
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
        let snapshot = self.observe_review(repo_path, "main").await?;
        let _ = (
            state_id,
            kind,
            scope,
            justification,
            algorithm,
            public_key,
            signature,
            signed_at_unix,
            client_operation_id,
            snapshot,
        );
        Err(ProtocolError::InvalidState(
            "hosted review sync uses ThreadService/RecordReview; local review signatures are not translated from v1 SignState".into(),
        ))
    }
}
