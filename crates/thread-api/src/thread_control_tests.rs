use crypto::Ed25519Signer;

use super::*;

fn observed(property: Property, parents: BTreeSet<ContentHash>) -> wire::ThreadOverview {
    let (kind, record_id) = property_key(&property);
    wire::ThreadOverview {
        r#ref: Some(wire::ThreadRef {
            spool: Some(wire::SpoolRef {
                id: Uuid::from_u128(1).to_string(),
            }),
            id: Some(wire::ThreadId {
                value: vec![11; 32],
            }),
        }),
        version: vec![8; 32],
        metadata_frontiers: vec![wire::ThreadPropertyFrontier {
            property: kind as i32,
            record_id,
            version: property_version(ContentHash::from_bytes([11; 32]), &property, &parents)
                .expect("property version")
                .as_bytes()
                .to_vec(),
            operation_ids: parents
                .into_iter()
                .map(|id| id.as_bytes().to_vec())
                .collect(),
        }],
        ..Default::default()
    }
}
fn author() -> Author<'static> {
    Author {
        account: Uuid::from_u128(2),
        agent_id: Some("agent-7"),
        authority_envelope: b"codec fixture: authorization is independently checked by receiver",
    }
}
#[test]
fn observed_frontier_prepares_original_signed_request_without_another_read() {
    let parents = BTreeSet::from([
        ContentHash::from_bytes([12; 32]),
        ContentHash::from_bytes([13; 32]),
    ]);
    let observed = observed(Property::Intent, parents.clone());
    let signer = Ed25519Signer::from_seed(&[19; 32]).expect("signer");
    let command = PreparedControl::sign(
        &observed,
        Control::Intent(Intent {
            outcome: "goal".into(),
            acceptance_criteria: vec!["tested".into()],
            origin_urls: vec![],
            principal_approved: false,
        }),
        author(),
        Uuid::from_u128(3),
        1000,
        &signer,
    )
    .expect("prepare from observed frontier");
    let request = command.revise_intent().expect("typed request");
    let operation =
        crate::replication::decode_record(request.operation.expect("original signature"))
            .expect("verify")
            .verify()
            .expect("decode");
    assert_eq!(operation.parents, parents);
    assert_eq!(
        request.expected_intent_version,
        observed.metadata_frontiers[0].version
    );
    assert_eq!(request.proposed_intent.expect("intent").agent_id, "agent-7");
    assert_eq!(request.client_operation_id, Uuid::from_u128(3).to_string());
    assert!(
        command.rename().is_err(),
        "a prepared control cannot change mutation kind"
    );
}
#[test]
fn incomplete_or_duplicate_frontier_never_produces_a_signature() {
    let mut observed = observed(
        Property::Name,
        BTreeSet::from([ContentHash::from_bytes([12; 32])]),
    );
    let signer = Ed25519Signer::from_seed(&[19; 32]).expect("signer");
    observed.metadata_frontiers[0].operation_ids.clear();
    assert!(
        PreparedControl::sign(
            &observed,
            Control::Name("new".into()),
            author(),
            Uuid::from_u128(3),
            1000,
            &signer
        )
        .err()
        .expect("parent/version mismatch")
        .to_string()
        .contains("exact parents")
    );
    let observed = wire::ThreadOverview {
        metadata_frontiers: vec![],
        ..observed
    };
    assert!(
        PreparedControl::sign(
            &observed,
            Control::Name("new".into()),
            author(),
            Uuid::from_u128(3),
            1000,
            &signer
        )
        .err()
        .expect("not observed")
        .to_string()
        .contains("observe")
    );
}
