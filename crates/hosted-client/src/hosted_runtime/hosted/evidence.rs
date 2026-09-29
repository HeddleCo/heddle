// SPDX-License-Identifier: Apache-2.0
//! Prepare original check authorship from the active hosted credential.
//! The same proof key signs the record and proves the delivery RPC.

use anyhow::{Context, Result, ensure};
use api::heddle::api::v1alpha2 as wire;
use crypto::{Ed25519Signer, Signer};
use objects::object::{ContentHash, thread_replication::metadata::AUTHORITY_FORMAT};
use thread_api::evidence::CheckAuthor;

use super::{HostedClient, resolve_hosted_credential};

pub async fn active_evidence_author(
    client: &HostedClient,
    server: &str,
) -> Result<(Ed25519Signer, CheckAuthor)> {
    let credential = resolve_hosted_credential(Some(server))?;
    let token = credential
        .token
        .context("hosted evidence requires a credential")?;
    let signer = Ed25519Signer::from_pem(
        credential
            .proof_key_pem
            .as_deref()
            .context("hosted evidence credential has no proof key")?,
    )?;
    let root = biscuit_verifier::unverified_authority_device_pop_key(&token.id)?
        .context("hosted evidence credential has no mint root")?;
    let parsed = biscuit_verifier::parse_token(&token.id, &[root])?;
    let inspected = biscuit_verifier::inspect_verified_credential(&parsed, &root)?;
    ensure!(
        inspected.proof_public_key == signer.public_key(),
        "hosted evidence proof key differs from active credential"
    );

    // GetIdentity seals the original credential's portable owner association.
    // It is in the runner template and does not grant a write or mint a root.
    let identity: wire::GetIdentityResponse = client
        .call_unary(
            "/heddle.api.v1alpha2.IdentityService/GetIdentity",
            &wire::GetIdentityRequest {
                include_current_credential: true,
            },
        )
        .await?;
    let principal = identity
        .identity
        .context("hosted evidence account absent")?;
    let current = identity
        .current_credential
        .context("hosted evidence credential absent")?;
    ensure!(
        current.proof_public_key == signer.public_key(),
        "GetIdentity returned another credential proof key"
    );
    ensure!(
        current.acting_agent_id == inspected.agent_id.as_deref().unwrap_or_default(),
        "GetIdentity returned another acting agent"
    );
    let account = uuid::Uuid::parse_str(&principal.account_id)
        .context("hosted evidence account ID must be a UUID")?;
    ensure!(
        inspected
            .asserted_account
            .is_none_or(|value| value == account),
        "hosted evidence credential belongs to another account"
    );
    ensure!(
        !current.thread_control_authority.is_empty(),
        "GetIdentity did not provide original RecordEvidence authority"
    );
    let publisher: [u8; 32] = signer
        .public_key()
        .try_into()
        .context("Ed25519 key length")?;
    let envelope = current.thread_control_authority;
    Ok((
        signer,
        CheckAuthor {
            actor: objects::object::CollaborationActor {
                principal_id: account,
                agent_id: inspected.agent_id,
            },
            publisher,
            authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &envelope),
            authority_envelope: envelope,
        },
    ))
}
