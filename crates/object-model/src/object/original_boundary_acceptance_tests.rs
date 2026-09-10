use super::*;
use crate::object::CollaborationActor;
fn hash(n: u8) -> ContentHash {
    ContentHash::from_bytes([n; 32])
}
fn fixture() -> (
    PublicationIntent,
    OriginalPublicationManifest,
    OriginalBoundaryAcceptance,
) {
    let spool = Uuid::from_u128(1);
    let account = Uuid::from_u128(2);
    let intent = PublicationIntent {
        spool,
        spool_genesis: hash(1),
        thread: hash(2),
        revision: StateId::from_bytes([3; 32]),
        inventory: hash(4),
        sharing_policy: None,
        source: [5; 32],
        destination: [6; 32],
        client_operation_id: Uuid::from_u128(3),
    };
    let original = OriginalManifestEntry {
        subject: ManifestSubject::Source(hash(7)),
        thread: intent.thread,
        publisher: [8; 32],
        authority: Some(OriginalAuthorityBinding {
            spool,
            actor: CollaborationActor {
                principal_id: account,
                agent_id: Some("offline-agent".into()),
            },
            authority_digest: hash(9),
        }),
    };
    let manifest = OriginalPublicationManifest::new(vec![original]).expect("manifest");
    let author = SourceAuthor::account(
        spool,
        CollaborationActor {
            principal_id: account,
            agent_id: None,
        },
        vec![10; 32],
    )
    .expect("current acceptance claim");
    let acceptance = OriginalBoundaryAcceptance {
        version: 1,
        publication_intent: intent.id().expect("intent"),
        originals_manifest: manifest.id().expect("manifest ID"),
        original_account: account,
        kinds: BTreeSet::from([BoundaryOriginalKind::Source]),
        accepting_publisher: [11; 32],
        accepting_author: author,
    };
    (intent, manifest, acceptance)
}
#[test]
fn boundary_acceptance_binds_every_original_and_exact_publication() {
    let (intent, manifest, acceptance) = fixture();
    assert_eq!(
        acceptance
            .selected(&intent, &manifest)
            .expect("exact selection"),
        vec![&manifest.entries[0]]
    );
    assert_eq!(
        OriginalBoundaryAcceptance::decode(&acceptance.encode().expect("encode")).expect("decode"),
        acceptance
    );
    assert_eq!(
        OriginalPublicationManifest::decode(&manifest.encode().expect("encode")).expect("decode"),
        manifest
    );
    let mut changed = manifest.clone();
    changed.entries[0]
        .authority
        .as_mut()
        .expect("author")
        .authority_digest = hash(12);
    assert!(
        acceptance.selected(&intent, &changed).is_err(),
        "changed original authority must invalidate exact manifest"
    );
    let mut extra = manifest.entries[0].clone();
    extra.subject = ManifestSubject::Source(hash(13));
    let changed = OriginalPublicationManifest::new(vec![manifest.entries[0].clone(), extra])
        .expect("larger valid manifest");
    assert!(
        acceptance.selected(&intent, &changed).is_err(),
        "extra signed original is not silently accepted"
    );
    let mut destination = intent.clone();
    destination.destination = [14; 32];
    assert!(
        acceptance.selected(&destination, &manifest).is_err(),
        "another endpoint cannot reuse fresh acceptance"
    );
    let mut operation = intent.clone();
    operation.client_operation_id = Uuid::from_u128(4);
    assert!(
        acceptance.selected(&operation, &manifest).is_err(),
        "different operation requires explicit acceptance"
    );
}
#[test]
fn boundary_manifest_rejects_duplicates_unsorted_and_bounds() {
    let (_, manifest, _) = fixture();
    let entry = manifest.entries[0].clone();
    assert!(OriginalPublicationManifest::new(vec![entry.clone(), entry.clone()]).is_err());
    let mut disguised = entry.clone();
    disguised.subject = ManifestSubject::OtherOperation(entry.subject.id());
    assert!(
        OriginalPublicationManifest::new(vec![entry.clone(), disguised]).is_err(),
        "operation facet relabel does not create a second identity"
    );
    let mut other = entry.clone();
    other.subject = ManifestSubject::Genesis(hash(1));
    other.thread = hash(1);
    other.authority.as_mut().expect("authority").actor.agent_id = None;
    let mut sorted = OriginalPublicationManifest::new(vec![entry.clone(), other]).expect("sorted");
    sorted.entries.reverse();
    assert!(sorted.encode().is_err());
    let entries = (0..=MAX_RECORDS)
        .map(|n| {
            let mut value = entry.clone();
            value.subject = ManifestSubject::Source(ContentHash::compute(&n.to_le_bytes()));
            value
        })
        .collect();
    assert!(
        OriginalPublicationManifest::new(entries).is_err(),
        "count bound must reject distinct originals"
    );
}
#[test]
fn boundary_acceptance_never_relabels_local_or_foreign_authors() {
    let (intent, mut manifest, mut acceptance) = fixture();
    let original = manifest.entries[0].clone();
    manifest.entries[0].authority = None;
    acceptance.originals_manifest = manifest.id().expect("local manifest");
    assert!(
        acceptance.selected(&intent, &manifest).is_err(),
        "local source has no account to infer"
    );
    manifest.entries[0] = original.clone();
    manifest.entries[0]
        .authority
        .as_mut()
        .expect("author")
        .actor
        .principal_id = Uuid::from_u128(77);
    acceptance.originals_manifest = manifest.id().expect("foreign manifest");
    assert!(
        acceptance.selected(&intent, &manifest).is_err(),
        "courier's account does not replace original account"
    );
    manifest.entries[0] = original.clone();
    acceptance.originals_manifest = manifest.id().expect("original manifest");
    let selected = acceptance
        .selected(&intent, &manifest)
        .expect("separate owner acceptance");
    assert_eq!(
        selected[0], &original,
        "original agent and credential digest stay exact"
    );
    acceptance.accepting_author = SourceAuthor::LocalKey;
    assert!(acceptance.encode().is_err());
}

#[test]
fn boundary_decode_accepts_maximum_canonical_authority() {
    let (intent, _, mut acceptance) = fixture();
    acceptance.accepting_author = SourceAuthor::account(
        intent.spool,
        CollaborationActor {
            principal_id: acceptance.original_account,
            agent_id: Some("a".repeat(256)),
        },
        vec![42; 64 * 1024],
    )
    .expect("maximum authority");
    let bytes = acceptance.encode().expect("encode maximum");
    assert_eq!(
        OriginalBoundaryAcceptance::decode(&bytes).expect("decode maximum"),
        acceptance
    );
}

#[test]
fn boundary_decode_accepts_maximum_manifest_count() {
    let (_, manifest, _) = fixture();
    let entries = (0..MAX_RECORDS)
        .map(|index| {
            let mut entry = manifest.entries[0].clone();
            entry.subject = ManifestSubject::Source(ContentHash::compute_typed(
                "boundary-limit-fixture",
                &index.to_le_bytes(),
            ));
            entry
        })
        .collect();
    let manifest = OriginalPublicationManifest::new(entries).expect("maximum manifest");
    let bytes = manifest.encode().expect("encode maximum manifest");
    assert_eq!(
        OriginalPublicationManifest::decode(&bytes).expect("decode maximum manifest"),
        manifest
    );
}
