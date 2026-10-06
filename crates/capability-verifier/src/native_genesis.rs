//! Native creator binding and independently selected StartThread authority.
use heddle_api::{heddle::api::v1alpha2 as wire, hybrid_codec::Reject, native_witness};

use crate::{
    Result,
    import_delegation::{Selection, native_lineage},
    thread_control_authority::{self, Context, Revocation},
};

/// Successful verification reports whether StartThread account authority was
/// checked or only a LocalKey binding (which still needs a hosted claim).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeOwnerKind {
    /// Original account StartThread authority was verified.
    Account,
    /// Creator binding only; hosted ownership requires a separate claim.
    LocalKey,
}
/// Typed result of portable native creator verification.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct NativeGenesisSummary {
    /// Digest of the exact signed creator binding.
    pub certificate_digest_hex: String,
    /// Immutable genesis owner variant that was verified.
    pub owner_kind: NativeOwnerKind,
    /// True when this result grants no account hosting authority.
    pub requires_hosting_claim: bool,
}

/// Verify the binding against independently verified owner and Spool history.
/// Boundary acceptance must additionally verify its current accepting authority.
pub fn verify_binding(
    binding: &wire::SignedNativeGenesisAuthorityV1,
    original: &wire::SignedRecord,
    envelope: &[u8],
    selection: &Selection<'_>,
) -> Result<()> {
    native_witness::verify_genesis_authority(binding, original, envelope)?;
    let body = binding.body.as_ref().ok_or(Reject::GenesisBinding)?;
    let (identity, chain, _) = native_lineage(selection)?;
    if body.identity.as_ref() != Some(&identity) || body.owner_chain_digest != chain {
        return Err(Reject::Root.into());
    }
    Ok(())
}

/// Verify original account creation at its witnessed admission time. LocalKey
/// creation requires the exact empty envelope and a separate hosted claim.
/// Independently selected actor state and durable mint admission inventory.
pub struct AuthorAuthority<'a> {
    /// The genesis account's own verified state.
    pub owner: &'a crate::VerifiedOwnerState,
    /// Exact pre-transition admissions, supplied by host state or authenticated witnesses.
    pub mint_roots: &'a [wire::SignedOwnerMintRootAttachment],
}

/// Verify the native binding and the separately resolved author's StartThread capability.
pub fn verify_original_authority(
    binding: &wire::SignedNativeGenesisAuthorityV1,
    original: &wire::SignedRecord,
    envelope: &[u8],
    selection: &Selection<'_>,
    author: AuthorAuthority<'_>,
    now_seconds: i64,
    revoked: impl Fn(Revocation<'_>) -> bool,
) -> Result<()> {
    verify_binding(binding, original, envelope, selection)?;
    selection
        .keyring
        .verify_current_owner(selection.owner, now_seconds, selection.limits)?;
    selection
        .owner
        .issuer_at(&selection.owner.state_hash(), now_seconds)?;
    let body = binding.body.as_ref().ok_or(Reject::GenesisBinding)?;
    if body.owner_kind == 1 {
        let identity = body.identity.as_ref().ok_or(Reject::Canonical)?;
        let account: &[u8; 16] = author
            .owner
            .signed_root()
            .root
            .as_ref()
            .ok_or(Reject::Root)?
            .account_uuid
            .as_slice()
            .try_into()
            .map_err(|_| Reject::Canonical)?;
        heddle_api::writer_authority::verify_account_binding(
            &heddle_api::writer_authority::decode_authority(envelope)?,
            account,
            &identity.owner_account_uuid,
            &selection.owner.owner_id(),
        )?;
        thread_control_authority::verify_genesis_with_retained_mint_roots(
            envelope,
            Context {
                owner: author.owner,
                account_uuid: account,
                publisher: body
                    .creator_public_key
                    .as_slice()
                    .try_into()
                    .map_err(|_| Reject::Canonical)?,
                agent_id: None,
                method: "/heddle.api.v1alpha2.ThreadService/StartThread",
                spool_path: &selection
                    .keyring
                    .wire()
                    .canonical_spool_path_segments
                    .join("/"),
                now: now_seconds,
            },
            author.mint_roots,
            revoked,
        )?;
    }
    Ok(())
}

/// Portable browser seam for original StartThread authority. The caller selects
/// lineage and current revocations independently; a bundle never enrolls roots.
#[allow(clippy::too_many_arguments)]
pub fn verify_bytes(
    binding: &[u8],
    original: &[u8],
    envelope: &[u8],
    keyring: &[u8],
    current_owner: &[u8],
    author_history: &[u8],
    admitted_mint_roots_json: &str,
    initial_owner: &[u8],
    spool_genesis: &[u8],
    revoked_keys_json: &str,
    revoked_credentials_json: &str,
    now: i64,
    max_ttl: i64,
) -> Result<NativeGenesisSummary> {
    let limits = crate::VerificationLimits::new(max_ttl)?;
    let (keyring, owner) = crate::observed::ownership(keyring, current_owner, now, limits)?;
    let initial = crate::canonical::fixed(initial_owner, "selected initial owner")?;
    let genesis = crate::canonical::fixed(spool_genesis, "selected Spool genesis")?;
    let binding: wire::SignedNativeGenesisAuthorityV1 = heddle_api::hybrid_codec::strict_decode(
        binding,
        heddle_api::import_authority::MAX_RECORD_BYTES,
    )?;
    let original: wire::SignedRecord = heddle_api::hybrid_codec::strict_decode(
        original,
        heddle_api::import_authority::MAX_RECORD_BYTES,
    )?;
    let mut lists = Vec::new();
    for json in [revoked_keys_json, revoked_credentials_json] {
        if json.len() > heddle_api::import_authority::MAX_BUNDLE_BYTES {
            return Err(Reject::Bounds.into());
        }
        let values: Vec<String> =
            serde_json::from_str(json).map_err(|e| crate::Error::Invalid(e.to_string()))?;
        if values.len() > 4096 {
            return Err(Reject::Bounds.into());
        }
        lists.push(values);
    }
    let keys = lists[0]
        .iter()
        .map(|v| {
            let bytes = hex::decode(v).map_err(|e| crate::Error::Invalid(e.to_string()))?;
            if bytes.len() != 32 {
                return Err(Reject::Canonical.into());
            }
            Ok(bytes)
        })
        .collect::<Result<Vec<_>>>()?;
    let history: wire::OwnerHistory =
        heddle_api::hybrid_codec::strict_decode(author_history, 65536)?;
    let author = crate::creation::history_state(&history, now)?;
    if admitted_mint_roots_json.len() > heddle_api::import_authority::MAX_BUNDLE_BYTES {
        return Err(Reject::Bounds.into());
    }
    let roots: Vec<String> = serde_json::from_str(admitted_mint_roots_json)
        .map_err(|e| crate::Error::Invalid(e.to_string()))?;
    if roots.len() > 256 {
        return Err(Reject::Bounds.into());
    }
    let mint_roots = roots
        .iter()
        .map(|hex| {
            let bytes = hex::decode(hex).map_err(|e| crate::Error::Invalid(e.to_string()))?;
            Ok(heddle_api::hybrid_codec::strict_decode(&bytes, 65536)?)
        })
        .collect::<Result<Vec<wire::SignedOwnerMintRootAttachment>>>()?;
    verify_original_authority(
        &binding,
        &original,
        envelope,
        &Selection {
            owner: &owner,
            keyring: &keyring,
            spool_genesis_digest: &genesis,
            initial_owner_id: &initial,
            limits,
        },
        AuthorAuthority {
            owner: &author,
            mint_roots: &mint_roots,
        },
        now,
        |r| match r {
            Revocation::MintRoot(key) | Revocation::Publisher(key) => {
                keys.contains(&heddle_api::hybrid_codec::key_id(key).to_vec())
            }
            Revocation::Credential(id) => lists[1].iter().any(|v| v == id),
        },
    )?;
    let owner_kind = match binding
        .body
        .as_ref()
        .ok_or(Reject::GenesisBinding)?
        .owner_kind
    {
        1 => NativeOwnerKind::Account,
        2 => NativeOwnerKind::LocalKey,
        _ => return Err(Reject::GenesisBinding.into()),
    };
    Ok(NativeGenesisSummary {
        certificate_digest_hex: hex::encode(native_witness::signed_genesis_digest(&binding)?),
        requires_hosting_claim: owner_kind == NativeOwnerKind::LocalKey,
        owner_kind,
    })
}
