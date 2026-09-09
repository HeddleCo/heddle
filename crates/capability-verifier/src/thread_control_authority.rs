//! Portable original-author evidence for signed Thread control operations.
//!
//! Incoming evidence never establishes trust. The host supplies independently
//! admitted current account authority and checks this gate at first admission.
//! A claimed author timestamp cannot revive an expired or revoked credential.
use biscuit_auth::{Biscuit, PublicKey};
use prost::Message;

use crate::{
    Error, Result, VerifiedOwnerState,
    wire::{OwnerHistory, SignedMintRootAttachment},
};

/// Maximum complete public proof, including owner history and sealed Biscuit.
pub const MAX_BYTES: usize = 64 * 1024;
use crate::wire::ThreadControlAuthority as Envelope;
fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// Construct bounded, sealed public evidence for an operation that the caller
/// will sign. This does not grant authority: first admission must call [`verify`]
/// and verify the canonical operation signature binding these exact bytes.
pub fn encode(
    owner: &OwnerHistory,
    mint_root_public_key: &[u8],
    attachment: Option<&SignedMintRootAttachment>,
    token: &Biscuit,
) -> Result<Vec<u8>> {
    if mint_root_public_key.len() != 32 {
        return Err(invalid("Thread authority mint root must be Ed25519"));
    }
    let sealed = match token.seal() {
        Ok(sealed) => sealed,
        Err(biscuit_auth::error::Token::AlreadySealed) => token.clone(),
        Err(error) => return Err(invalid(format!("seal Thread authority: {error}"))),
    };
    let envelope = Envelope {
        format: 1,
        owner: Some(owner.clone()),
        mint_root_public_key: mint_root_public_key.to_vec(),
        mint_root_attachment: attachment.cloned(),
        sealed_biscuit: sealed
            .to_vec()
            .map_err(|error| invalid(error.to_string()))?,
    };
    if envelope.encoded_len() > MAX_BYTES {
        return Err(Error::TooLarge { limit: MAX_BYTES });
    }
    Ok(envelope.encode_to_vec())
}

/// Independently admitted context. Every value is resolved by the host or taken
/// from the already verified canonical operation, never trusted from its proof.
pub struct Context<'a> {
    /// Current authority from authenticated account observation or local pin.
    pub owner: &'a VerifiedOwnerState,
    /// Exact signed actor's account UUID, compared with the admitted account.
    pub account_uuid: &'a [u8; 16],
    /// Key that verified the original canonical Thread control signature.
    pub publisher: &'a [u8; 32],
    /// Exact signed agent attribution; `None` means a human operation.
    pub agent_id: Option<&'a str>,
    /// Exact Thread property mutation method selected by the host.
    pub method: &'a str,
    /// Current canonical Spool path resolved by the host.
    pub spool_path: &'a str,
    /// Actual admission clock; never the author's claimed occurrence time.
    pub now: i64,
}
/// Original accountable author, authorized at the actual admission boundary.
pub struct VerifiedAuthor {
    /// Cryptographically bound account UUID.
    pub account_uuid: [u8; 16],
    /// Original operation publisher key.
    pub publisher: [u8; 32],
    /// Original verified agent attribution.
    pub agent_id: Option<String>,
    /// Normal shared capability facts, including expiry and all revocation IDs.
    pub facts: heddle_biscuit_verifier::BiscuitFacts,
}
/// Typed revocation lookups supplied by the independently admitted host state.
#[derive(Clone, Copy)]
pub enum Revocation<'a> {
    /// Biscuit session or signed block revocation identifier.
    Credential(&'a str),
    /// Original signing/minting root key.
    MintRoot(&'a [u8]),
    /// Original operation publisher/PoP key.
    Publisher(&'a [u8]),
}
/// Verify current original authority without network, storage or clock access.
/// Delivery authorization is a separate host gate. Previously accepted records
/// may use their exact durable acceptance; this function never infers one from
/// author timestamps, current delivery credentials, or supplied owner history.
pub fn verify(
    bytes: &[u8],
    context: Context<'_>,
    is_revoked: impl Fn(Revocation<'_>) -> bool,
) -> Result<VerifiedAuthor> {
    verify_with_retained_mint_roots(bytes, context, &[], is_revoked)
}

/// As [`verify`], with exact certificates from independently trusted durable
/// admission. These survive ordinary owner rotation without recertification;
/// matching only a certificate's key or author-supplied timestamp is insufficient.
pub fn verify_with_retained_mint_roots(
    bytes: &[u8],
    context: Context<'_>,
    admitted_mint_roots: &[SignedMintRootAttachment],
    is_revoked: impl Fn(Revocation<'_>) -> bool,
) -> Result<VerifiedAuthor> {
    verify_original(bytes, context, admitted_mint_roots, is_revoked, true)
}

/// Genesis binds creator key and owner, while its agent attribution comes from
/// the verified original capability. No courier-supplied agent label participates.
pub fn verify_genesis_with_retained_mint_roots(
    bytes: &[u8],
    context: Context<'_>,
    admitted_mint_roots: &[SignedMintRootAttachment],
    is_revoked: impl Fn(Revocation<'_>) -> bool,
) -> Result<VerifiedAuthor> {
    if !matches!(
        context.method,
        "/heddle.api.v2alpha1.ThreadService/StartThread"
            | "/heddle.api.v2alpha1.IntegrationService/ImportSource"
    ) {
        return Err(invalid(
            "genesis authority requires an exact creation method",
        ));
    }
    verify_original(bytes, context, admitted_mint_roots, is_revoked, false)
}

fn verify_original(
    bytes: &[u8],
    context: Context<'_>,
    admitted_mint_roots: &[SignedMintRootAttachment],
    is_revoked: impl Fn(Revocation<'_>) -> bool,
    bind_agent_attribution: bool,
) -> Result<VerifiedAuthor> {
    if admitted_mint_roots.len() > 256 {
        return Err(invalid("retained mint certificate inventory exceeds bound"));
    }
    if bytes.len() > MAX_BYTES {
        return Err(Error::TooLarge { limit: MAX_BYTES });
    }
    let envelope = Envelope::decode(bytes)
        .map_err(|error| invalid(format!("Thread authority encoding: {error}")))?;
    if envelope.format != 1
        || envelope.encode_to_vec() != bytes
        || envelope.mint_root_public_key.len() != 32
        || envelope.sealed_biscuit.is_empty()
    {
        return Err(invalid("invalid or noncanonical Thread authority envelope"));
    }
    if !matches!(
        context.method,
        "/heddle.api.v2alpha1.ThreadService/RenameThread"
            | "/heddle.api.v2alpha1.ThreadService/ReviseIntent"
            | "/heddle.api.v2alpha1.ThreadService/ChangeLifecycle"
            | "/heddle.api.v2alpha1.ThreadService/SetSharingPolicy"
            | "/heddle.api.v2alpha1.ThreadService/SetAudiencePolicy"
            | "/heddle.api.v2alpha1.ThreadService/SetRetentionPolicy"
            | "/heddle.api.v2alpha1.ThreadService/StartThread"
            | "/heddle.api.v2alpha1.IntegrationService/ImportSource"
            | "/heddle.api.v2alpha1.SyncService/PublishContent"
            | "/heddle.api.v2alpha1.ThreadService/ClaimThreadOwnership"
            | "/heddle.api.v2alpha1.ThreadService/RecordReview"
            | "/heddle.api.v2alpha1.EvidenceService/RecordEvidence"
            | "/heddle.api.v2alpha1.EvidenceService/AcknowledgeCheck"
    ) || context.spool_path.is_empty()
        || context.spool_path.len() > 4096
    {
        return Err(invalid(
            "Thread authority requires the exact mutation and resolved Spool path",
        ));
    }
    let history = envelope
        .owner
        .as_ref()
        .ok_or_else(|| invalid("original owner history required"))?;
    let original = crate::creation::history_state(history, context.now)?;
    let root = original
        .signed_root()
        .root
        .as_ref()
        .ok_or_else(|| invalid("owner root missing"))?;
    if !context.owner.extends(&original)
        || original.owner_id() != context.owner.owner_id()
        || root.account_uuid != context.account_uuid
    {
        return Err(invalid(
            "Thread author history is not a verified prefix of independently admitted current account authority",
        ));
    }
    if envelope.mint_root_public_key != context.owner.authority_key().public_key {
        let attachment = envelope
            .mint_root_attachment
            .as_ref()
            .ok_or_else(|| invalid("mint root requires current owner attachment"))?;
        if admitted_mint_roots.contains(attachment) {
            crate::creation::verify_retained_mint_root_attachment(
                attachment,
                context.owner,
                context.account_uuid,
                &envelope.mint_root_public_key,
                context.now,
            )?;
        } else {
            crate::creation::verify_mint_root_attachment(
                attachment,
                context.owner,
                context.account_uuid,
                &envelope.mint_root_public_key,
                context.now,
            )?;
        }
    } else if envelope.mint_root_attachment.is_some() {
        return Err(invalid(
            "direct owner mint root must not carry an unrelated attachment",
        ));
    }
    if is_revoked(Revocation::MintRoot(&envelope.mint_root_public_key))
        || is_revoked(Revocation::Publisher(context.publisher))
    {
        return Err(invalid("original Thread mint root or publisher is revoked"));
    }
    let key = PublicKey::from_bytes(
        &envelope.mint_root_public_key,
        biscuit_auth::Algorithm::Ed25519,
    )
    .map_err(|error| invalid(error.to_string()))?;
    let token = Biscuit::from(&envelope.sealed_biscuit, key)
        .map_err(|error| invalid(format!("original Thread credential: {error}")))?;
    if !matches!(token.seal(), Err(biscuit_auth::error::Token::AlreadySealed)) {
        return Err(invalid("public Thread authority must be sealed"));
    }
    let now = chrono::DateTime::from_timestamp(context.now, 0)
        .ok_or_else(|| invalid("invalid admission time"))?;
    let operation = context
        .method
        .rsplit('/')
        .next()
        .ok_or_else(|| invalid("mutation required"))?;
    let facts = heddle_biscuit_verifier::authorize_at(
        &token,
        operation,
        now,
        None,
        &[],
        Some(("spool", context.spool_path)),
    )
    .map_err(|error| invalid(format!("original Thread authorization: {error}")))?;
    let publisher = hex::encode(context.publisher);
    if facts.cnf.as_deref() != Some(publisher.as_str())
        || facts
            .subject_user_id()
            .is_some_and(|id| id.as_bytes() != context.account_uuid)
    {
        return Err(invalid(
            "Thread publisher or account differs from original capability",
        ));
    }
    if facts
        .revocation_identities()
        .any(|id| is_revoked(Revocation::Credential(id)))
    {
        return Err(invalid("original Thread capability is revoked"));
    }
    let agent = facts.delegation_agent_id.clone().or_else(|| {
        (facts.agent_provider.is_some() || facts.agent_model.is_some()).then(|| facts.sid.clone())
    });
    if bind_agent_attribution && agent.as_deref() != context.agent_id {
        return Err(invalid(
            "Thread agent attribution differs from original capability",
        ));
    }
    Ok(VerifiedAuthor {
        account_uuid: *context.account_uuid,
        publisher: *context.publisher,
        agent_id: agent,
        facts,
    })
}
