//! Provenance inspection and explicit present-tense acceptance are different
//! typed results. An original may be expired/revoked; its accepting authority
//! must pass the unchanged current capability engine and all revocations.
use biscuit_auth::{Biscuit, PublicKey, builder::BlockBuilder};

use crate::{
    Error, Result, VerifiedOwnerState,
    thread_control_authority::{self as original, Context, Revocation, VerifiedAuthor},
    wire::SignedMintRootAttachment,
};

/// Verifier-owned request selector. Credentials may check, never assert it.
pub const REQUEST_PREDICATE: &str = "heddle_boundary_acceptance_request_v1";
/// Mutation purpose selected from the verified original's canonical kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundarySubjectKind {
    /// Account-authored Capture or LocalIntegration.
    Source,
    /// Account-owned original genesis.
    AccountGenesis,
    /// Explicit dual-signed ownership claim.
    OwnershipClaim,
}
impl BoundarySubjectKind {
    fn method(self) -> &'static str {
        match self {
            Self::Source => "/heddle.api.v2alpha1.SyncService/PublishContent",
            Self::AccountGenesis => "/heddle.api.v2alpha1.ThreadService/StartThread",
            Self::OwnershipClaim => "/heddle.api.v2alpha1.ThreadService/ClaimThreadOwnership",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::AccountGenesis => "account_genesis",
            Self::OwnershipClaim => "ownership_claim",
        }
    }
}
/// Exact original identity selected from the signed manifest, not a courier label.
pub struct OriginalSubjectScope<'a> {
    /// Resolves the existing mutation method; never a caller-selected method name.
    pub kind: BoundarySubjectKind,
    /// Original immutable account, equal to the accepting account.
    pub account: &'a [u8; 16],
    /// Original Thread identity.
    pub thread: &'a [u8; 32],
    /// Exact original canonical record identity.
    pub subject: &'a [u8; 32],
    /// Key that signed the original, possibly now revoked.
    pub publisher: &'a [u8; 32],
    /// Original agent attribution; absent for human authors.
    pub agent_id: Option<&'a str>,
}
/// An authenticated assertion, never proof of prior or current action authority.
/// Explicit owner-derived acceptance can preserve revoked-agent work without
/// treating that agent's revoked proof as its own present authorization.
pub struct InspectedOriginalIdentity {
    /// Independently pinned account whose historical proof was inspected.
    pub account_uuid: [u8; 16],
    /// Verified effective proof key, never a current authorization result.
    pub publisher: [u8; 32],
    /// Authenticated original attribution, unchanged by acceptance.
    pub agent_id: Option<String>,
    /// Declared expiry if present; arbitrary time checks are not reinterpreted.
    pub credential_expiry: Option<u64>,
    /// Session, credential, and every signed block selector for audit.
    pub revocation_identities: Vec<String>,
    /// Current revocation observation; does not erase historical identity.
    pub explicitly_revoked: bool,
}
/// Caller must first verify the immutable original record signature and digest.
/// Current owner history is independently pinned; incoming proof never enrolls
/// a root. The clock validates signed owner transitions, not old Biscuit rights.
pub fn inspect_original_identity(
    bytes: &[u8],
    context: Context<'_>,
    is_revoked: impl Fn(Revocation<'_>) -> bool,
) -> Result<InspectedOriginalIdentity> {
    let envelope = original::decode_envelope(bytes)?;
    let original_owner = original::verify_original_owner(&envelope, &context)?;
    inspect_mint_provenance(
        &envelope,
        &original_owner,
        context.owner,
        context.account_uuid,
    )?;
    let key = PublicKey::from_bytes(
        &envelope.mint_root_public_key,
        biscuit_auth::Algorithm::Ed25519,
    )
    .map_err(invalid)?;
    let token = Biscuit::from(&envelope.sealed_biscuit, key).map_err(invalid)?;
    if !matches!(token.seal(), Err(biscuit_auth::error::Token::AlreadySealed)) {
        return Err(invalid("original identity proof must be sealed"));
    }
    let facts =
        heddle_biscuit_verifier::inspect_verified_credential(&token, &key).map_err(invalid)?;
    if facts.proof_public_key != context.publisher
        || facts
            .asserted_account
            .is_some_and(|account| account.as_bytes() != context.account_uuid)
        || facts.agent_id.as_deref() != context.agent_id
    {
        return Err(invalid("inspected identity differs from signed original"));
    }
    let identifiers: Vec<String> = facts.revocation_identities().map(str::to_owned).collect();
    let revoked = is_revoked(Revocation::MintRoot(&envelope.mint_root_public_key))
        || is_revoked(Revocation::Publisher(context.publisher))
        || identifiers
            .iter()
            .any(|id| is_revoked(Revocation::Credential(id)));
    Ok(InspectedOriginalIdentity {
        account_uuid: *context.account_uuid,
        publisher: *context.publisher,
        agent_id: facts.agent_id,
        credential_expiry: (facts.expires_at_unix_seconds > 0)
            .then_some(facts.expires_at_unix_seconds),
        revocation_identities: identifiers,
        explicitly_revoked: revoked,
    })
}
fn inspect_mint_provenance(
    envelope: &crate::wire::ThreadControlAuthority,
    original_owner: &VerifiedOwnerState,
    current: &VerifiedOwnerState,
    account: &[u8; 16],
) -> Result<()> {
    if envelope.mint_root_public_key == original_owner.authority_key().public_key {
        if envelope.mint_root_attachment.is_some() {
            return Err(invalid("direct original root has unrelated attachment"));
        }
        return Ok(());
    }
    let signed = envelope
        .mint_root_attachment
        .as_ref()
        .ok_or_else(|| invalid("original mint identity attachment required"))?;
    let body = signed
        .attachment
        .as_ref()
        .ok_or_else(|| invalid("original mint attachment missing"))?;
    let issuer = current.provenance_issuer(&body.owner_state_hash, body.owner_sequence)?;
    if body.account_uuid != account
        || body.owner_key.as_ref() != Some(issuer)
        || body
            .mint_root_key
            .as_ref()
            .is_none_or(|key| key.public_key != envelope.mint_root_public_key)
    {
        return Err(invalid(
            "original mint provenance differs from independently pinned account",
        ));
    }
    let canonical = crate::creation::canonical_mint_root_attachment(body)?;
    crate::crypto::verify_signature(
        issuer,
        signed
            .owner_signature
            .as_ref()
            .ok_or_else(|| invalid("original mint signature missing"))?,
        crate::creation::MINT_ROOT_DOMAIN,
        &canonical,
    )
}
/// Verify current explicit acceptance with all existing capability restrictions.
/// This returns current accepting authority, never relabels the original actor.
/// Hosts must separately apply current resource grants/audiences and atomically
/// retain the exact signed acceptance/receipt at the same authorization epoch.
pub fn verify_accepting_authority(
    bytes: &[u8],
    context: Context<'_>,
    subject: OriginalSubjectScope<'_>,
    retained: &[SignedMintRootAttachment],
    is_revoked: impl Fn(Revocation<'_>) -> bool,
) -> Result<VerifiedAuthor> {
    if subject.account != context.account_uuid
        || subject.kind.method() != context.method
        || subject.thread == &[0; 32]
        || subject.subject == &[0; 32]
        || subject.publisher == &[0; 32]
        || subject
            .agent_id
            .is_some_and(|id| id.is_empty() || id.len() > 256 || id.chars().any(char::is_control))
    {
        return Err(invalid(
            "acceptance differs from exact original subject scope",
        ));
    }
    // One composite request fact prevents mixing selectors from different originals.
    // JSON string encoding is also the existing Biscuit string literal encoding.
    let agent = serde_json::to_string(&subject.agent_id.unwrap_or("")).map_err(invalid)?;
    let fact = format!(
        "{REQUEST_PREDICATE}(\"{}\", \"{}\", \"{}\", \"{}\", \"{}\", {agent})",
        subject.kind.label(),
        hex::encode(subject.account),
        hex::encode(subject.thread),
        hex::encode(subject.subject),
        hex::encode(subject.publisher)
    );
    original::verify_original(bytes, context, retained, is_revoked, true, &[fact])
}
pub(super) fn reject_request_claims(token: &Biscuit) -> Result<()> {
    for index in 0..token.block_count() {
        let source = token.print_block_source(index).map_err(invalid)?;
        let block = BlockBuilder::new().code(&source).map_err(invalid)?;
        if block
            .facts
            .iter()
            .any(|fact| fact.predicate.name == REQUEST_PREDICATE)
            || block
                .rules
                .iter()
                .any(|rule| rule.head.name == REQUEST_PREDICATE)
        {
            return Err(invalid(
                "credential asserts reserved boundary acceptance request",
            ));
        }
    }
    Ok(())
}
fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}
