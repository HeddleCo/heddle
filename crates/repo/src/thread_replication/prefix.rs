//! Prefix projection changes only carrier arrays and unsigned selectors. All
//! original, admission and publication bytes remain exactly as authenticated.
use std::collections::BTreeSet;

use api::{
    heddle::api::v1alpha2 as wire,
    hybrid_codec::{self, Reject},
    import_authority as import,
};
use prost::Message;

use super::authority::PublicProof;

impl PublicProof {
    pub fn foreign_dependencies(&self) -> &[wire::ForeignDependencyV1] {
        match self {
            Self::Import(b) => &b.foreign_dependencies,
            Self::Native(b) => &b.foreign_dependencies,
        }
    }

    pub fn foreign_original(
        &self,
        reference: &wire::ForeignDependencyV1,
    ) -> Result<&wire::SignedRecord, Reject> {
        let (authority, landing) = match self {
            Self::Import(b) => (&b.authority_witnesses, &b.landing_witnesses),
            Self::Native(b) => (&b.authority_witnesses, &b.landing_witnesses),
        };
        authority
            .iter()
            .flat_map(|p| &p.dependencies)
            .chain(
                landing
                    .iter()
                    .flat_map(|p| p.source_operation.iter().chain(&p.review_evidence)),
            )
            .find(|r| {
                import::signed_native_digest(r).is_ok_and(|d| d == reference.signed_native_digest)
            })
            .ok_or(Reject::Scope)
    }
    pub fn prefix_original(
        &self,
        reference: &wire::ForeignDependencyV1,
    ) -> Result<&wire::SignedRecord, Reject> {
        let (genesis, authority, landing) = match self {
            Self::Import(b) => (
                b.genesis_witnesses
                    .iter()
                    .filter_map(|p| p.original_genesis.as_ref())
                    .collect::<Vec<_>>(),
                &b.authority_witnesses,
                &b.landing_witnesses,
            ),
            Self::Native(b) => (
                b.genesis_witnesses
                    .iter()
                    .filter_map(|p| p.original_genesis.as_ref())
                    .collect::<Vec<_>>(),
                &b.authority_witnesses,
                &b.landing_witnesses,
            ),
        };
        genesis
            .iter()
            .copied()
            .chain(
                authority
                    .iter()
                    .flat_map(|p| p.original.iter().chain(&p.dependencies)),
            )
            .chain(landing.iter().flat_map(|p| {
                p.execution
                    .iter()
                    .chain(p.source_operation.iter())
                    .chain(&p.review_evidence)
            }))
            .find(|r| {
                import::signed_native_digest(r).is_ok_and(|d| d == reference.signed_native_digest)
            })
            .ok_or(Reject::Scope)
    }

    // A complete acceptance may select an original admitted after this cutoff.
    // Until the contract defines proof-only originals, that prefix cannot travel.
    fn require_boundary_originals(&self) -> Result<(), Reject> {
        use objects::object::{
            original_boundary_acceptance::{
                OriginalBoundaryAcceptance, OriginalPublicationManifest, PublicationIntent,
            },
            thread_replication::{
                self as native, ownership_claim::ThreadOwnershipClaim,
                ownership_resolution::ThreadOwnershipResolution,
            },
        };
        let (geneses, authority, landing, boundaries) = match self {
            Self::Native(b) => (
                b.genesis_witnesses
                    .iter()
                    .filter_map(|p| p.original_genesis.as_ref())
                    .collect::<Vec<_>>(),
                &b.authority_witnesses,
                &b.landing_witnesses,
                b.genesis_witnesses
                    .iter()
                    .filter_map(|p| p.boundary_acceptance.as_ref())
                    .collect::<Vec<_>>(),
            ),
            Self::Import(b) => (
                b.genesis_witnesses
                    .iter()
                    .filter_map(|p| p.original_genesis.as_ref())
                    .collect::<Vec<_>>(),
                &b.authority_witnesses,
                &b.landing_witnesses,
                b.genesis_witnesses
                    .iter()
                    .filter_map(|p| p.boundary_acceptance.as_ref())
                    .collect::<Vec<_>>(),
            ),
        };
        let originals = geneses
            .into_iter()
            .chain(
                authority
                    .iter()
                    .flat_map(|p| p.original.iter().chain(&p.dependencies)),
            )
            .chain(landing.iter().flat_map(|p| {
                p.execution
                    .iter()
                    .chain(p.source_operation.iter())
                    .chain(&p.review_evidence)
            }));
        let mut ids = BTreeSet::new();
        for original in originals {
            let id = match original.format.as_str() {
                native::GENESIS_FORMAT => crypto::import_authority::verify_native_genesis(original)
                    .map_err(|_| Reject::Scope)?
                    .1
                    .id(),
                native::OPERATION_FORMAT => {
                    crypto::import_authority::verify_native_operation(original)
                        .map_err(|_| Reject::Scope)?
                        .1
                        .id()
                }
                native::ownership_claim::FORMAT => {
                    ThreadOwnershipClaim::decode(&original.canonical_record)
                        .map_err(|_| Reject::Scope)?
                        .id()
                }
                native::ownership_resolution::FORMAT => {
                    ThreadOwnershipResolution::decode(&original.canonical_record)
                        .map_err(|_| Reject::Scope)?
                        .id()
                }
                _ => continue,
            }
            .map_err(|_| Reject::Scope)?;
            ids.insert(id);
        }
        for boundary in boundaries
            .into_iter()
            .chain(authority.iter().flat_map(|p| &p.boundary_acceptances))
        {
            let acceptance = OriginalBoundaryAcceptance::decode(
                &boundary
                    .signed_acceptance
                    .as_ref()
                    .ok_or(Reject::Scope)?
                    .canonical_record,
            )
            .map_err(|_| Reject::Scope)?;
            let manifest = OriginalPublicationManifest::decode(&boundary.originals_manifest)
                .map_err(|_| Reject::Scope)?;
            let intent = PublicationIntent::decode(&boundary.publication_intent)
                .map_err(|_| Reject::Scope)?;
            if acceptance
                .selected(&intent, &manifest)
                .map_err(|_| Reject::Scope)?
                .iter()
                .any(|entry| !ids.contains(&entry.subject.id()))
            {
                return Err(Reject::Scope);
            }
        }
        Ok(())
    }

    /// Select the requested own-origin prefix before traversing its foreign
    /// obligations. The receiver still verifies the exact installed endpoint
    /// and derived cutoff inside its transaction.
    pub fn prefix(&self, reference: &wire::ForeignDependencyV1) -> Result<Self, Reject> {
        if reference.format_version != 1
            || reference.prefix_admission_order == 0
            || reference.thread_genesis_digest.len() != 32
            || reference.signed_native_digest.len() != 32
        {
            return Err(Reject::Scope);
        }
        let cutoff = reference.prefix_admission_order;
        let mut projected = self.clone();
        match &mut projected {
            Self::Native(b) if reference.origin == 2 => {
                api::native_witness::validate_public_bundle(b)?;
                b.statements
                    .retain(|s| s.body.as_ref().is_some_and(|s| s.admission_order <= cutoff));
                let payloads = payloads(&b.statements)?;
                retain(&mut b.genesis_witnesses, &payloads)?;
                retain(&mut b.authority_witnesses, &payloads)?;
                retain(&mut b.landing_witnesses, &payloads)?;
                retain_foreign(
                    &mut b.foreign_dependencies,
                    &b.authority_witnesses,
                    &b.landing_witnesses,
                )?;
                api::native_witness::validate_public_bundle(b)?;
            }
            Self::Import(b) if reference.origin == 1 => {
                import::validate_public_bundle(b)?;
                b.statements
                    .retain(|s| s.body.as_ref().is_some_and(|s| s.admission_order <= cutoff));
                let payloads = payloads(&b.statements)?;
                retain(&mut b.genesis_witnesses, &payloads)?;
                retain(&mut b.authority_witnesses, &payloads)?;
                retain(&mut b.landing_witnesses, &payloads)?;
                let mut selected = BTreeSet::new();
                let mut manifests = BTreeSet::new();
                for operation in &b.operations {
                    for manifest in &b.manifests {
                        let publication = import::publication_payload(operation, manifest)?;
                        if payloads.contains(&hybrid_codec::canonical(&publication)?) {
                            selected.insert(import::signed_operation_digest(operation)?);
                            manifests.insert(import::manifest_digest(manifest)?);
                        }
                    }
                }
                b.operations.retain(|o| {
                    import::signed_operation_digest(o).is_ok_and(|d| selected.contains(&d))
                });
                b.manifests
                    .retain(|m| import::manifest_digest(m).is_ok_and(|d| manifests.contains(&d)));
                b.terminal_manifest = b.manifests.iter().max_by_key(|m| m.slots.len()).cloned();
                retain_foreign(
                    &mut b.foreign_dependencies,
                    &b.authority_witnesses,
                    &b.landing_witnesses,
                )?;
                import::validate_public_bundle(b)?;
            }
            _ => return Err(Reject::Scope),
        }
        projected.require_boundary_originals()?;
        Ok(projected)
    }
}
fn payloads(
    statements: &[api::heddle::api::common::SignedHostedWitnessStatementV1],
) -> Result<BTreeSet<Vec<u8>>, Reject> {
    statements
        .iter()
        .map(|s| {
            Ok(s.body
                .as_ref()
                .ok_or(Reject::Canonical)?
                .canonical_payload
                .clone())
        })
        .collect()
}
fn retain<T: Message + hybrid_codec::Canonical>(
    records: &mut Vec<T>,
    payloads: &BTreeSet<Vec<u8>>,
) -> Result<(), Reject> {
    let mut keep = Vec::with_capacity(records.len());
    for record in records.drain(..) {
        if payloads.contains(&hybrid_codec::canonical(&record)?) {
            keep.push(record);
        }
    }
    *records = keep;
    Ok(())
}
fn retain_foreign(
    references: &mut Vec<wire::ForeignDependencyV1>,
    authority: &[wire::ImportAuthorityWitnessV1],
    landing: &[wire::HostedLandingWitnessV1],
) -> Result<(), Reject> {
    let used = authority
        .iter()
        .flat_map(|p| &p.dependencies)
        .chain(
            landing
                .iter()
                .flat_map(|p| p.source_operation.iter().chain(&p.review_evidence)),
        )
        .map(import::signed_native_digest)
        .collect::<Result<BTreeSet<_>, _>>()?;
    references.retain(|r| used.contains(&r.signed_native_digest));
    Ok(())
}
