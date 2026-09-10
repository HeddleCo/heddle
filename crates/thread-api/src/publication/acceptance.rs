//! Explicit publication proposals. Signature/manifest validation is deliberately
//! separate from the host's current accepting-authority and ownership checks.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use crypto::{Signer, original_boundary_acceptance::SignedBoundaryAcceptance};
use heddle_object_model::object::{
    ContentHash, StateId,
    original_boundary_acceptance::{
        AdmissionBasis, BoundaryOriginalKind, FORMAT, ManifestSubject, OriginalBoundaryAcceptance,
        OriginalManifestEntry, OriginalPublicationManifest, PublicationIntent,
    },
    thread_replication::{SourceAuthor, ThreadGenesis},
};
use prost::Message;
use uuid::Uuid;

use super::PublicationOriginals;
use crate::{contract::*, transport::Error};

/// An immutable, signature-checked selection, NOT current permission. A host
/// must inspect original provenance and verify the accepting capability for
/// every selected subject under its current resource/ownership fence.
pub struct ProposedAcceptance {
    signed: Arc<SignedBoundaryAcceptance>,
    value: OriginalBoundaryAcceptance,
    subjects: BTreeSet<ManifestSubject>,
    manifest: Arc<OriginalPublicationManifest>,
}
impl ProposedAcceptance {
    /// Return only an exact selected descriptor from the full signed manifest.
    /// The host compares rebuilt original identity/envelope to this value before
    /// current acceptance verification; an original ID alone is insufficient for
    /// the unsigned creator-envelope sidecar of an Account genesis.
    pub fn entry(&self, subject: &ManifestSubject) -> Option<&OriginalManifestEntry> {
        if !self.subjects.contains(subject) {
            return None;
        }
        self.manifest
            .entries
            .binary_search_by(|entry| entry.subject.cmp(subject))
            .ok()
            .map(|index| &self.manifest.entries[index])
    }
    pub fn signed(&self) -> &Arc<SignedBoundaryAcceptance> {
        &self.signed
    }
    pub fn value(&self) -> &OriginalBoundaryAcceptance {
        &self.value
    }
    pub fn subjects(&self) -> &BTreeSet<ManifestSubject> {
        &self.subjects
    }
}

/// Full immutable publication selection shared by the client and host. Retained
/// receipts are verified separately and cannot silently become fresh proposals.
pub struct PublicationAcceptancePlan {
    intent: PublicationIntent,
    manifest: Arc<OriginalPublicationManifest>,
    proposed: BTreeMap<ContentHash, ProposedAcceptance>,
    subjects: BTreeMap<ManifestSubject, ContentHash>,
}
impl PublicationAcceptancePlan {
    pub fn intent(&self) -> &PublicationIntent {
        &self.intent
    }
    pub fn manifest(&self) -> &OriginalPublicationManifest {
        &self.manifest
    }
    pub fn proposed(&self) -> &BTreeMap<ContentHash, ProposedAcceptance> {
        &self.proposed
    }
    pub fn acceptance_for(&self, subject: &ManifestSubject) -> Option<&ProposedAcceptance> {
        self.subjects
            .get(subject)
            .and_then(|id| self.proposed.get(id))
    }
    fn insert(&mut self, signed: SignedBoundaryAcceptance) -> Result<ContentHash, Error> {
        if self.proposed.len() >= crate::boundary_acceptance::MAX_ACCEPTANCES {
            return Err(Error::Protocol("fresh acceptance count exceeded"));
        }
        let value = signed.verify_signature().map_err(invalid)?;
        let id = value.id().map_err(invalid)?;
        if self.proposed.contains_key(&id) {
            return Err(Error::Protocol("duplicate fresh acceptance"));
        }
        let selected = value
            .selected(&self.intent, &self.manifest)
            .map_err(invalid)?;
        let subjects: BTreeSet<_> = selected.iter().map(|entry| entry.subject.clone()).collect();
        if subjects
            .iter()
            .any(|subject| self.subjects.contains_key(subject))
        {
            return Err(Error::Protocol("overlapping fresh acceptance selection"));
        }
        for subject in &subjects {
            self.subjects.insert(subject.clone(), id);
        }
        self.proposed.insert(
            id,
            ProposedAcceptance {
                signed: Arc::new(signed),
                value,
                subjects,
                manifest: self.manifest.clone(),
            },
        );
        Ok(id)
    }
}

/// Owned preparation for an explicit local prepare/sign/send workflow. Preparing
/// or signing never refreshes identity, contacts Weft, or modifies an original.
pub struct PreparedPublication {
    opening: PublishContentClientFrame,
    originals: PublicationOriginals,
    plan: PublicationAcceptancePlan,
}
impl PreparedPublication {
    pub fn new(
        opening: PublishContentClientFrame,
        originals: PublicationOriginals,
        spool_genesis: ContentHash,
    ) -> Result<Self, Error> {
        // Ordinary inputs only. Callers deliberately attach a new signature with
        // sign_acceptance/accept; unreferenced incoming evidence is not trusted.
        let plan = prepare_plan(&opening, &originals, spool_genesis)?;
        Ok(Self {
            opening,
            originals,
            plan,
        })
    }
    pub fn opening(&self) -> &PublishContentClientFrame {
        &self.opening
    }
    pub fn originals(&self) -> &PublicationOriginals {
        &self.originals
    }
    pub fn plan(&self) -> &PublicationAcceptancePlan {
        &self.plan
    }
    /// Build an unsigned exact intent for an external signer. The caller chooses
    /// the accepting authority explicitly; this is no claim of current validity.
    pub fn acceptance(
        &self,
        author: SourceAuthor,
        publisher: [u8; 32],
        kinds: BTreeSet<BoundaryOriginalKind>,
    ) -> Result<OriginalBoundaryAcceptance, Error> {
        let SourceAuthor::Account { actor, .. } = &author else {
            return Err(Error::Protocol(
                "explicit account accepting authority required",
            ));
        };
        let value = OriginalBoundaryAcceptance {
            version: 1,
            publication_intent: self.plan.intent.id().map_err(invalid)?,
            originals_manifest: self.plan.manifest.id().map_err(invalid)?,
            original_account: actor.principal_id,
            kinds,
            accepting_publisher: publisher,
            accepting_author: author,
        };
        value
            .selected(&self.plan.intent, &self.plan.manifest)
            .map_err(invalid)?;
        Ok(value)
    }
    /// Explicitly sign a separate acceptance. Original signatures and cached
    /// author envelopes remain byte-for-byte unchanged, including revoked work.
    pub fn sign_acceptance(
        &mut self,
        author: SourceAuthor,
        kinds: BTreeSet<BoundaryOriginalKind>,
        signer: &impl Signer,
    ) -> Result<ContentHash, Error> {
        let publisher = signer
            .public_key()
            .try_into()
            .map_err(|_| Error::Protocol("invalid accepting key"))?;
        let value = self.acceptance(author, publisher, kinds)?;
        self.accept(SignedBoundaryAcceptance::sign(&value, signer).map_err(invalid)?)
    }
    /// Attach a signature produced by an external signer. Reject a changed
    /// intent, extra originals, ambiguous selection, or exceeding carrier bounds.
    pub fn accept(&mut self, signed: SignedBoundaryAcceptance) -> Result<ContentHash, Error> {
        let wire = crate::boundary_acceptance::encode(&signed)?;
        let wire_len = wire.encoded_len() + 10; // field key + maximum length varint
        let total: usize = self
            .originals
            .geneses
            .iter()
            .map(Message::encoded_len)
            .chain(self.originals.operations.iter().map(Message::encoded_len))
            .sum();
        let existing: BTreeSet<_> = self
            .originals
            .geneses
            .iter()
            .flat_map(|g| &g.boundary_acceptances)
            .chain(
                self.originals
                    .operations
                    .iter()
                    .flat_map(|b| &b.boundary_acceptances),
            )
            .map(|record| record.canonical_record.as_slice())
            .collect();
        if existing.contains(wire.canonical_record.as_slice()) {
            return Err(Error::Protocol(
                "duplicate fresh acceptance or retained evidence",
            ));
        }
        if total.saturating_add(wire_len) > 16 * 1024 * 1024 || existing.len() >= 128 {
            return Err(Error::Protocol(
                "publication acceptance metadata budget exceeded",
            ));
        }
        // Choose an existing original carrier. Never add an empty operation
        // batch or copy the 16 MiB original collection just to attach a proof.
        let wrapper = self
            .originals
            .geneses
            .iter()
            .position(|g| g.encoded_len().saturating_add(wire_len) <= 256 * 1024);
        let batch = self
            .originals
            .operations
            .iter()
            .position(|b| b.encoded_len().saturating_add(wire_len) <= 256 * 1024);
        if wrapper.is_none() && batch.is_none() {
            return Err(Error::Protocol("no bounded acceptance carrier available"));
        }
        let id = self.plan.insert(signed)?;
        if let Some(index) = wrapper {
            self.originals.geneses[index]
                .boundary_acceptances
                .push(wire);
        } else if let Some(index) = batch {
            self.originals.operations[index]
                .boundary_acceptances
                .push(wire);
        }
        Ok(id)
    }
    pub fn into_parts(
        self,
    ) -> (
        PublishContentClientFrame,
        PublicationOriginals,
        PublicationAcceptancePlan,
    ) {
        (self.opening, self.originals, self.plan)
    }
}

/// Publication-only candidate extraction. Removes only unreferenced acceptance
/// carriers from the owned input, leaving generic receipt matching strict.
/// Full signature, manifest, and intent checks finish before a proposal is exposed.
pub fn proposed_publication(
    opening: &PublishContentClientFrame,
    mut originals: PublicationOriginals,
    spool_genesis: ContentHash,
) -> Result<(PublicationOriginals, PublicationAcceptancePlan), Error> {
    originals.validate_bounds().map_err(invalid)?;
    let mut candidates = BTreeMap::new();
    let mut retained = BTreeSet::new();
    for wrapper in &mut originals.geneses {
        let mut references = BTreeSet::new();
        if let Some(receipt) = &wrapper.admission {
            let value = heddle_object_model::object::thread_genesis_admission::ThreadGenesisAdmission::decode(&receipt.canonical_record).map_err(invalid)?;
            reference(&value.basis, &mut references);
        }
        for receipt in &wrapper.ownership_claim_admissions {
            reference(
                &crate::authority_admission::verify_signature(receipt)?.basis,
                &mut references,
            );
        }
        separate(
            &mut wrapper.boundary_acceptances,
            &references,
            &mut retained,
            &mut candidates,
        )?;
    }
    for batch in &mut originals.operations {
        let mut references = BTreeSet::new();
        for receipt in &batch.authority_admissions {
            reference(
                &crate::authority_admission::verify_signature(receipt)?.basis,
                &mut references,
            );
        }
        separate(
            &mut batch.boundary_acceptances,
            &references,
            &mut retained,
            &mut candidates,
        )?;
    }
    if candidates.keys().any(|id| retained.contains(id)) {
        return Err(Error::Protocol(
            "acceptance cannot be both fresh and retained evidence",
        ));
    }
    let mut plan = prepare_plan(opening, &originals, spool_genesis)?;
    for (_, signed) in candidates {
        plan.insert(signed)?;
    }
    Ok((originals, plan))
}
fn reference(basis: &AdmissionBasis, ids: &mut BTreeSet<ContentHash>) {
    if let AdmissionBasis::BoundaryAcceptance { acceptance } = basis {
        ids.insert(*acceptance);
    }
}
fn separate(
    records: &mut Vec<SignedRecord>,
    references: &BTreeSet<ContentHash>,
    retained: &mut BTreeSet<ContentHash>,
    candidates: &mut BTreeMap<ContentHash, SignedBoundaryAcceptance>,
) -> Result<(), Error> {
    let mut local = BTreeSet::new();
    let mut keep = Vec::new();
    for wire in std::mem::take(records) {
        let signed = crate::boundary_acceptance::decode(&wire)?;
        let id = ContentHash::compute_typed(FORMAT, &signed.canonical);
        if !local.insert(id) {
            return Err(Error::Protocol(
                "duplicate acceptance in publication carrier",
            ));
        }
        if references.contains(&id) {
            retained.insert(id);
            keep.push(wire);
        } else if candidates.insert(id, signed).is_some() {
            return Err(Error::Protocol("duplicate fresh acceptance"));
        }
    }
    *records = keep;
    Ok(())
}
fn prepare_plan(
    opening: &PublishContentClientFrame,
    originals: &PublicationOriginals,
    spool_genesis: ContentHash,
) -> Result<PublicationAcceptancePlan, Error> {
    originals.validate_bounds().map_err(invalid)?;
    let intent = publication_intent(opening, spool_genesis)?;
    let mut entries = Vec::new();
    let mut geneses = BTreeSet::new();
    for wrapper in &originals.geneses {
        let wire = wrapper
            .genesis
            .as_ref()
            .ok_or(Error::Protocol("missing original genesis"))?;
        let decoded = ThreadGenesis::decode(&wire.canonical_record).map_err(invalid)?;
        let id = decoded.id().map_err(invalid)?;
        let reference = ThreadRef {
            spool: Some(SpoolRef {
                id: intent.spool.to_string(),
            }),
            id: Some(ThreadId {
                value: id.as_bytes().to_vec(),
            }),
        };
        let genesis = crate::fetch::verify_origin(wrapper, &reference).map_err(invalid)?;
        if !geneses.insert(id) {
            return Err(Error::Protocol("duplicate original genesis"));
        }
        entries.push(
            OriginalManifestEntry::from_genesis(&genesis, &wrapper.creator_authority)
                .map_err(invalid)?,
        );
        for claim in crate::replication::ownership::verify_claims(wrapper, &genesis)? {
            entries.push(
                OriginalManifestEntry::from_claim(&claim.original.verify().map_err(invalid)?)
                    .map_err(invalid)?,
            );
        }
    }
    if !geneses.contains(&intent.thread) {
        return Err(Error::Protocol("selected original genesis missing"));
    }
    for batch in &originals.operations {
        for received in crate::authority_admission::match_batch(batch)? {
            let operation = received.original.verify().map_err(invalid)?;
            if !geneses.contains(&operation.thread) {
                return Err(Error::Protocol("operation original genesis missing"));
            }
            let entry = OriginalManifestEntry::from_operation(&operation).map_err(invalid)?;
            if entry
                .authority
                .as_ref()
                .is_some_and(|authority| authority.spool != intent.spool)
            {
                return Err(Error::Protocol(
                    "original authority differs from publication Spool",
                ));
            }
            entries.push(entry);
        }
    }
    let manifest = OriginalPublicationManifest::new(entries).map_err(invalid)?;
    Ok(PublicationAcceptancePlan {
        intent,
        manifest: Arc::new(manifest),
        proposed: BTreeMap::new(),
        subjects: BTreeMap::new(),
    })
}
/// Shared exact immutable intent. Checkpoint is a transport cursor, excluded so
/// a resume does not require re-signing; the operation ID remains committed.
pub fn publication_intent(
    opening: &PublishContentClientFrame,
    spool_genesis: ContentHash,
) -> Result<PublicationIntent, Error> {
    let Some(publish_content_client_frame::Body::Open(open)) = &opening.body else {
        return Err(Error::Protocol("publication Open required"));
    };
    let thread = open
        .thread
        .as_ref()
        .ok_or(Error::Protocol("publication Thread required"))?;
    let spool = thread
        .spool
        .as_ref()
        .ok_or(Error::Protocol("publication Spool required"))?;
    let revision = open
        .revision
        .as_ref()
        .ok_or(Error::Protocol("publication revision required"))?;
    if revision.spool.as_ref() != Some(spool) {
        return Err(Error::Protocol("publication revision Spool differs"));
    }
    let Some(revision_ref::Revision::State(state)) = &revision.revision else {
        return Err(Error::Protocol("exact publication State required"));
    };
    let hash = |bytes: &[u8]| -> Result<ContentHash, Error> {
        let value: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::Protocol("32-byte publication identity required"))?;
        if value == [0; 32] {
            return Err(Error::Protocol("zero publication identity"));
        }
        Ok(ContentHash::from_bytes(value))
    };
    let endpoint = |endpoint: Option<&EndpointRef>| -> Result<[u8; 32], Error> {
        endpoint
            .ok_or(Error::Protocol("publication endpoint required"))?
            .public_key
            .as_slice()
            .try_into()
            .map_err(|_| Error::Protocol("32-byte endpoint required"))
    };
    if spool_genesis.as_bytes() == &[0; 32] {
        return Err(Error::Protocol("verified Spool genesis required"));
    }
    let value = PublicationIntent {
        spool: Uuid::parse_str(&spool.id).map_err(invalid)?,
        spool_genesis,
        thread: hash(
            &thread
                .id
                .as_ref()
                .ok_or(Error::Protocol("Thread ID required"))?
                .value,
        )?,
        revision: StateId::from_bytes(*hash(&state.value)?.as_bytes()),
        inventory: ContentHash::from_bytes(super::inventory_digest(&open.packs).map_err(invalid)?),
        sharing_policy: if open.sharing_policy_version.is_empty() {
            None
        } else {
            Some(hash(&open.sharing_policy_version)?)
        },
        source: endpoint(open.source.as_ref())?,
        destination: endpoint(open.destination.as_ref())?,
        client_operation_id: Uuid::parse_str(&opening.client_operation_id).map_err(invalid)?,
    };
    value.id().map_err(invalid)?;
    Ok(value)
}
fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Io(error.to_string())
}

#[cfg(test)]
#[path = "acceptance_tests.rs"]
mod tests;
