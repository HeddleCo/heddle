//! Unmodified alpha.21 conformance bytes; expected signatures are never minted.
use std::{
    future::Future,
    pin::pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use api::{
    heddle::api::common::{SignedHostedWitnessSetV1, SignedHostedWitnessStatementV1},
    witness_trust::{SetExpectation, verify_set},
};

use super::history::*;
use crate::contract::{GetHostedWitnessHistoryProofRequest, GetHostedWitnessHistoryProofResponse};

fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha21.json"))
        .expect("fixed fixture")
}
fn wire<T: prost::Message + Default>(section: &str, name: &str) -> T {
    let v = fixture();
    T::decode(
        hex::decode(v[section][name]["wire_hex"].as_str().expect("wire hex"))
            .expect("hex")
            .as_slice(),
    )
    .expect("fixed protobuf")
}
fn set(name: &str) -> (api::witness_trust::VerifiedWitnessSet, i64) {
    let v = fixture();
    let signed: SignedHostedWitnessSetV1 = wire("signed_vectors", name);
    let now = signed
        .body
        .as_ref()
        .expect("set body")
        .issued_at_unix_millis
        + 1;
    let root = hex::decode(
        v["keys"]["root"]["public_key_hex"]
            .as_str()
            .expect("root key"),
    )
    .expect("root");
    let expected = SetExpectation {
        authority: v["context"]["authority"].as_str().expect("authority"),
        root_id: v["context"]["root_id"].as_str().expect("root ID"),
        root_public_key: &root,
        root_epoch: 1,
        now_unix_millis: now,
        clock_floor_unix_millis: 0,
        known_job_keys: &[],
    };
    (
        verify_set(&signed, &expected, None).expect("independently authenticated set"),
        now,
    )
}
fn ready<F: Future>(future: F) -> F::Output {
    match pin!(future)
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("fixture future must complete immediately"),
    }
}
struct Lookup {
    response: Option<GetHostedWitnessHistoryProofResponse>,
    requests: Arc<Mutex<Vec<GetHostedWitnessHistoryProofRequest>>>,
}
impl HistoryProofLookup for Lookup {
    type Error = std::io::Error;
    async fn lookup(
        &self,
        request: &GetHostedWitnessHistoryProofRequest,
    ) -> Result<Option<GetHostedWitnessHistoryProofResponse>, Self::Error> {
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        Ok(self.response.clone())
    }
}

#[test]
fn retrospective_lookup_returns_only_the_exact_originals_retirement_proof() {
    let (set, now) = set("retired_set");
    let signed: SignedHostedWitnessStatementV1 = wire("signed_vectors", "publication_statement");
    let lookup = Lookup {
        response: Some(wire("wire_vectors", "lookup_response")),
        requests: Default::default(),
    };
    let proof = ready(retrieve(&lookup, &set, &signed, now)).expect("retained committed history");
    assert_eq!(proof, wire("wire_vectors", "publication_proof"));
    assert_eq!(
        *lookup.requests.lock().expect("requests"),
        [wire("wire_vectors", "lookup_request")]
    );
    // The request has only selectors/digest. It needs no credential or resource.
    assert_eq!(
        prost::Message::encoded_len(&request(&signed).expect("request")),
        68
    );
}

#[test]
fn proof_substitution_cannot_authorize_a_neighboring_original() {
    let (set, now) = set("retired_set");
    let signed: SignedHostedWitnessStatementV1 = wire("signed_vectors", "genesis_admission");
    let mut lookup = Lookup {
        response: Some(wire("wire_vectors", "lookup_response")),
        requests: Default::default(),
    };
    assert!(matches!(
        ready(retrieve(&lookup, &set, &signed, now)),
        Err(Error::Rejected(api::hybrid_codec::Reject::Proof))
    ));
    lookup.response = Some(GetHostedWitnessHistoryProofResponse {
        proof: Some(wire("wire_vectors", "genesis_proof")),
    });
    ready(retrieve(&lookup, &set, &signed, now)).expect("matching genesis proof");
}

#[test]
fn lookup_misses_are_uniform_and_malformed_selectors_never_reach_lookup() {
    let (set, now) = set("retired_set");
    let lookup = Lookup {
        response: None,
        requests: Default::default(),
    };
    for name in ["genesis_admission", "publication_statement"] {
        let signed = wire("signed_vectors", name);
        assert!(matches!(
            ready(retrieve(&lookup, &set, &signed, now)),
            Err(Error::NotFound)
        ));
    }
    let mut signed: SignedHostedWitnessStatementV1 = wire("signed_vectors", "genesis_admission");
    signed.body.as_mut().expect("body").executor_id.pop();
    assert!(ready(retrieve(&lookup, &set, &signed, now)).is_err());
    assert_eq!(lookup.requests.lock().expect("requests").len(), 2);
}

#[test]
fn learned_revocation_and_changed_original_invalidate_a_resolved_context() {
    let (current, now) = set("current_set");
    let signed: SignedHostedWitnessStatementV1 = wire("signed_vectors", "publication_statement");
    let resolved = api::witness_trust::resolve_statement(&current, &signed, None, false, now)
        .expect("current history");
    api::witness_trust::recheck_context(&resolved, &current, &signed, now)
        .expect("unchanged context");
    let (revoked, later) = set("revoked_set");
    assert!(api::witness_trust::recheck_context(&resolved, &revoked, &signed, later).is_err());
    assert_eq!(
        api::witness_trust::resolve_statement(&revoked, &signed, None, false, later)
            .expect_err("revoked history"),
        api::hybrid_codec::Reject::Revoked
    );
    let mut changed = signed.clone();
    changed.signature[0] ^= 1;
    assert!(api::witness_trust::recheck_context(&resolved, &current, &changed, now).is_err());
}

#[test]
fn receiver_refresh_preserves_every_original_and_rejects_authority_substitution() {
    let mut original: crate::contract::ImportPublicProofBundleV1 =
        wire("wire_vectors", "complete_renewed_export");
    let before = original.clone();
    let mut refreshed = original.clone();
    refreshed.witness_set = Some(wire("signed_vectors", "revoked_set"));
    replace_receiver_metadata(&mut original, refreshed.clone())
        .expect("receiver metadata refresh is structural, never authority");
    assert_eq!(original.original_geneses, before.original_geneses);
    assert_eq!(original.genesis_authorities, before.genesis_authorities);
    assert_eq!(original.delegations, before.delegations);
    assert_eq!(original.operations, before.operations);
    assert_eq!(original.statements, before.statements);
    assert_eq!(original.witness_set, refreshed.witness_set);
    // Proof-only refresh cannot replace retained signed authority bytes.
    let mut substituted = refreshed.clone();
    substituted.statements[0].signature[0] ^= 1;
    let unchanged = original.clone();
    assert_eq!(
        replace_receiver_metadata(&mut original, substituted),
        Err(api::hybrid_codec::Reject::Scope)
    );
    assert_eq!(original, unchanged);
    replace_receiver_metadata(&mut original, refreshed).expect("exact originals remain usable");
}
