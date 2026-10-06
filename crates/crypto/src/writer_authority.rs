//! Actor authority is independent of Spool governance. Hosts resolve installed
//! account state and durable attachments; receivers resolve exact witnessed
//! envelopes without enrolling their roots as future account authority.
use std::collections::BTreeMap;

use api::{
    heddle::api::{common as host, v1alpha2 as wire},
    hybrid_codec::{self, Reject},
    writer_authority::{self as contract, WriterWitnessPayload},
};
use heddleco_capability_verifier::{VerifiedOwnerState, creation};
use prost::Message;

use crate::import_authority::{Result, WitnessEvidence};

pub struct AuthorState {
    pub owner: VerifiedOwnerState,
    pub mint_roots: Vec<wire::SignedOwnerMintRootAttachment>,
}

/// Single-account host context. The inventory must come from durable enrollment,
/// never from the incoming envelope or a receiver's witness testimony.
pub struct HostAuthorAuthority<'a> {
    pub owner: &'a VerifiedOwnerState,
    pub mint_roots: &'a [wire::SignedOwnerMintRootAttachment],
}
impl HostAuthorAuthority<'_> {
    pub fn resolve(&self, account: &[u8; 16], _: &[u8], _: i64) -> Result<AuthorState> {
        if self
            .owner
            .signed_root()
            .root
            .as_ref()
            .ok_or(Reject::Root)?
            .account_uuid
            != account
        {
            return Err(Reject::Root.into());
        }
        Ok(AuthorState {
            owner: self.owner.clone(),
            mint_roots: self.mint_roots.to_vec(),
        })
    }
}

#[derive(Default)]
/// Receiver inventories contain only attachments admitted by exact witness evidence.
/// Host enrollment cannot be substituted for receiver testimony.
/// ```compile_fail
/// use crypto::writer_authority::{HostAuthorAuthority, WitnessedAuthors};
/// fn substitute(host: &HostAuthorAuthority<'_>) {
///     let _: &WitnessedAuthors = host;
/// }
/// ```
pub struct WitnessedAuthors {
    envelopes: BTreeMap<Vec<u8>, Vec<wire::SignedOwnerMintRootAttachment>>,
}
impl WitnessedAuthors {
    pub fn from_native(
        bundle: &wire::NativePublicProofBundleV1,
        set: &api::witness_trust::VerifiedWitnessSet,
        now: i64,
    ) -> Result<Self> {
        let mut authors = Self::default();
        for signed in &bundle.statements {
            let s = signed.body.as_ref().ok_or(Reject::Canonical)?;
            let evidence = resolve(signed, &bundle.history_proofs, set, now)?;
            let payload = match s.purpose {
                1 => WriterWitnessPayload::NativeGenesis(
                    bundle
                        .genesis_witnesses
                        .iter()
                        .find(|p| {
                            hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload)
                        })
                        .ok_or(Reject::Scope)?,
                ),
                2 => {
                    WriterWitnessPayload::Import(api::import_authority::WitnessPayload::Authority(
                        bundle
                            .authority_witnesses
                            .iter()
                            .find(|p| {
                                hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload)
                            })
                            .ok_or(Reject::Scope)?,
                    ))
                }
                4 => WriterWitnessPayload::Import(api::import_authority::WitnessPayload::Landing(
                    bundle
                        .landing_witnesses
                        .iter()
                        .find(|p| {
                            hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload)
                        })
                        .ok_or(Reject::Scope)?,
                )),
                _ => return Err(Reject::Version.into()),
            };
            authors.admit(&evidence, payload)?;
        }
        Ok(authors)
    }
    pub fn from_import(
        bundle: &wire::ImportPublicProofBundleV1,
        set: &api::witness_trust::VerifiedWitnessSet,
        now: i64,
    ) -> Result<Self> {
        let mut authors = Self::default();
        for signed in &bundle.statements {
            let s = signed.body.as_ref().ok_or(Reject::Canonical)?;
            let evidence = resolve(signed, &bundle.history_proofs, set, now)?;
            let payload = match s.purpose {
                1 => api::import_authority::WitnessPayload::Genesis(
                    bundle
                        .genesis_witnesses
                        .iter()
                        .find(|p| {
                            hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload)
                        })
                        .ok_or(Reject::Scope)?,
                ),
                2 => api::import_authority::WitnessPayload::Authority(
                    bundle
                        .authority_witnesses
                        .iter()
                        .find(|p| {
                            hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload)
                        })
                        .ok_or(Reject::Scope)?,
                ),
                3 => continue,
                4 => api::import_authority::WitnessPayload::Landing(
                    bundle
                        .landing_witnesses
                        .iter()
                        .find(|p| {
                            hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload)
                        })
                        .ok_or(Reject::Scope)?,
                ),
                _ => return Err(Reject::Version.into()),
            };
            authors.admit(&evidence, WriterWitnessPayload::Import(payload))?;
        }
        Ok(authors)
    }
    /// Evidence must have been resolved against independently selected current
    /// witness trust. API extraction binds the attachment to this exact payload,
    /// selecting the signed acceptor for basis 2.
    pub fn admit(
        &mut self,
        evidence: &WitnessEvidence,
        payload: WriterWitnessPayload<'_>,
    ) -> Result<()> {
        let statement = evidence.signed().body.as_ref().ok_or(Reject::Canonical)?;
        let (original, boundary) = match &payload {
            WriterWitnessPayload::NativeGenesis(p) => (
                p.creator_authority_envelope.as_slice(),
                p.boundary_acceptance.as_ref(),
            ),
            WriterWitnessPayload::Import(api::import_authority::WitnessPayload::Genesis(p)) => (
                p.creator_authority_envelope.as_slice(),
                p.boundary_acceptance.as_ref(),
            ),
            WriterWitnessPayload::Import(api::import_authority::WitnessPayload::Authority(p)) => (
                p.authority_envelope.as_slice(),
                p.boundary_acceptances
                    .iter()
                    .find(|b| b.binding == statement.boundary_acceptance),
            ),
            WriterWitnessPayload::Import(api::import_authority::WitnessPayload::Landing(p)) => {
                (p.authority_envelope.as_slice(), None)
            }
        };
        let accepting;
        let envelope = if statement.basis == 2 {
            let signed = boundary
                .and_then(|b| b.signed_acceptance.as_ref())
                .ok_or(Reject::BoundaryAcceptance)?;
            if signed.signatures.len() != 1 {
                return Err(Reject::Signature.into());
            }
            let acceptance = crate::original_boundary_acceptance::SignedBoundaryAcceptance {
                canonical: signed.canonical_record.clone(),
                signature: signed.signatures[0].signature.clone(),
            }
            .verify_signature()?;
            if signed.signatures[0].public_key != acceptance.accepting_publisher {
                return Err(Reject::Signature.into());
            }
            let heddle_object_model::object::thread_replication::SourceAuthor::Account {
                authority,
                ..
            } = acceptance.accepting_author
            else {
                return Err(Reject::BoundaryAcceptance.into());
            };
            accepting = authority;
            accepting.as_slice()
        } else {
            original
        };
        if envelope.is_empty() {
            return Ok(());
        }
        // Import member permissions retain their separate, verified job scope.
        if statement.purpose == 1
            && envelope.starts_with(b"heddle-signed-import-member-permission-v1\0")
        {
            return Ok(());
        }
        let authority = contract::decode_authority(envelope)?;
        let history = authority.owner.as_ref().ok_or(Reject::Root)?;
        creation::history_state(history, statement.observed_at_unix_millis / 1000)?;
        let roots = if let Some(
            wire::thread_control_authority::MintRootAssociation::OwnerMintRootAttachment(a),
        ) = &authority.mint_root_association
        {
            let body = a.attachment.as_ref().ok_or(Reject::Root)?;
            let issuer = contract::retained_mint_root_issuer(
                history,
                &body.owner_state_hash,
                body.owner_sequence,
            )?;
            let admitted = contract::admitted_owner_mint_root_attachment(statement, payload)?;
            contract::verify_retained_writer_attachment(
                &a.encode_to_vec(),
                &authority.mint_root_public_key,
                &issuer,
                &admitted,
                statement.observed_at_unix_millis / 1000,
            )?;
            vec![a.clone()]
        } else {
            vec![]
        };
        self.envelopes.insert(envelope.to_vec(), roots);
        Ok(())
    }
}
fn resolve(
    signed: &host::SignedHostedWitnessStatementV1,
    proofs: &[host::HostedWitnessHistoryProofV1],
    set: &api::witness_trust::VerifiedWitnessSet,
    now: i64,
) -> Result<WitnessEvidence> {
    match WitnessEvidence::resolve(set, signed, None, false, now) {
        Ok(e) => Ok(e),
        Err(crate::import_authority::Error::Contract(Reject::Proof)) => {
            for proof in proofs {
                if let Ok(e) = WitnessEvidence::resolve(set, signed, Some(proof), false, now) {
                    return Ok(e);
                }
            }
            Err(Reject::Proof.into())
        }
        Err(e) => Err(e),
    }
}
impl WitnessedAuthors {
    pub fn resolve(
        &self,
        account: &[u8; 16],
        envelope: &[u8],
        pinned: &VerifiedOwnerState,
        now: i64,
    ) -> Result<AuthorState> {
        let mint_roots = self.envelopes.get(envelope).ok_or(Reject::Scope)?.clone();
        let authority = contract::decode_authority(envelope)?;
        let history = authority.owner.as_ref().ok_or(Reject::Root)?;
        let owner = creation::history_state(history, now)?;
        if owner
            .signed_root()
            .root
            .as_ref()
            .ok_or(Reject::Root)?
            .account_uuid
            != account
        {
            return Err(Reject::GenesisBinding.into());
        }
        let owner = if pinned
            .signed_root()
            .root
            .as_ref()
            .ok_or(Reject::Root)?
            .account_uuid
            == account
        {
            // QA7: the receiver's selected newer state must extend this envelope.
            if !pinned.extends(&owner) {
                return Err(Reject::Root.into());
            }
            pinned.clone()
        } else {
            owner
        };
        for attachment in &mint_roots {
            let body = attachment.attachment.as_ref().ok_or(Reject::Root)?;
            creation::verify_retained_mint_root_attachment(
                attachment,
                &owner,
                account,
                &body.mint_root_key.as_ref().ok_or(Reject::Root)?.public_key,
                now,
            )?;
        }
        Ok(AuthorState { owner, mint_roots })
    }
}
