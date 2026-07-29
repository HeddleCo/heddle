use std::fmt::Debug;

use heddle_client::owner_authorization::{
    AuthorizationError, AuthorizationKey, CloneKeyringStore, create_anonymous_credential,
    create_anonymous_registration, create_clone_keyring, create_deferred_bootstrap,
    create_deferred_owner_root, create_human_bootstrap, create_rotation_transition,
    verify_anonymous_registration, verify_authorization_bundle, verify_bootstrap_response,
    verify_deferred_bootstrap, verify_owner_root, wire::*,
};
use prost::Message;
use tempfile::TempDir;

use crate::common::{Fixture, NOW, limits};

fn round_trip<T>(value: &T) -> T
where
    T: Message + Default + PartialEq + Debug,
{
    let bytes = value.encode_to_vec();
    let decoded = T::decode(bytes.as_slice()).expect("protobuf decodes");
    assert_eq!(&decoded, value);
    decoded
}

#[test]
fn owner_root_and_human_bootstrap_objects_round_trip_and_verify() {
    let fixture = Fixture::new();
    let signed_root = round_trip(&fixture.root);
    let root = signed_root.root.as_ref().expect("root");
    round_trip(root);
    round_trip(root.authority_key.as_ref().expect("authority key"));
    round_trip(
        signed_root
            .authority_proof
            .as_ref()
            .expect("authority proof"),
    );
    let policy = root.recovery_policy.as_ref().expect("policy");
    round_trip(policy);
    round_trip(&policy.guardians[0]);
    verify_owner_root(&signed_root).expect("root verifies after serialization");

    let new_passkey = NewPasskeyOwnerRootApproval {
        client_data_json: b"client-data".to_vec(),
        attestation_object: b"attestation".to_vec(),
    };
    round_trip(&new_passkey);
    let existing_passkey = ExistingPasskeyOwnerRootApproval {
        credential_id: b"credential".to_vec(),
        client_data_json: b"client-data".to_vec(),
        authenticator_data: b"authenticator".to_vec(),
        signature: b"passkey-signature".to_vec(),
    };
    round_trip(&existing_passkey);
    let approval = WebAuthnOwnerRootApproval {
        challenge_id: "challenge-1".to_string(),
        proof: Some(web_authn_owner_root_approval::Proof::NewPasskey(
            new_passkey,
        )),
    };
    round_trip(&approval);
    round_trip(&WebAuthnOwnerRootApproval {
        challenge_id: "challenge-2".to_string(),
        proof: Some(web_authn_owner_root_approval::Proof::ExistingPasskey(
            existing_passkey,
        )),
    });
    let bootstrap =
        create_human_bootstrap(fixture.root.clone(), approval, "bootstrap-op".to_string())
            .expect("bootstrap request");
    round_trip(&bootstrap);
    let bootstrap_response = BootstrapOwnerRootResponse {
        owner_id: fixture.state.owner_id().to_vec(),
        accepted_root_hash: fixture.state.state_hash().to_vec(),
    };
    verify_bootstrap_response(&round_trip(&bootstrap_response), &fixture.root)
        .expect("bootstrap receipt matches");
}

#[test]
fn transition_and_capability_objects_round_trip_and_verify() {
    let fixture = Fixture::new();
    let next = AuthorizationKey::from_seed([20; 32]).expect("next key");
    let transition = create_rotation_transition(
        &fixture.state,
        &fixture.authority,
        &next,
        NOW,
        NOW + 20,
        limits(),
    )
    .expect("rotation");
    round_trip(transition.transition.as_ref().expect("transition body"));
    round_trip(&transition);

    let capability = fixture.direct(&[SpoolCapabilityAction::Read, SpoolCapabilityAction::Grant]);
    let capability_body = capability.capability.as_ref().expect("capability");
    round_trip(capability_body.subject.as_ref().expect("subject"));
    round_trip(&capability_body.grants[0]);
    round_trip(capability_body.grants[0].spool.as_ref().expect("selector"));
    round_trip(capability_body);
    round_trip(&capability);
    let bundle = fixture.bundle(capability.clone());
    round_trip(&bundle);
    verify_authorization_bundle(&round_trip(&bundle), NOW, limits())
        .expect("authorization bundle verifies");
    let submit = SubmitOwnerAuthorizationRequest {
        authorization: Some(bundle.clone()),
        client_operation_id: "submit-op".to_string(),
    };
    round_trip(&submit);
    round_trip(&SubmitOwnerAuthorizationResponse {
        capability_id: capability_body.capability_id.clone(),
        expires_at_unix_seconds: capability_body.expires_at_unix_seconds,
    });
}

#[test]
fn anonymous_and_clone_keyring_objects_round_trip_and_verify() {
    let fixture = Fixture::new();
    let anonymous_key = AuthorizationKey::from_seed([21; 32]).expect("anonymous");
    let credential = create_anonymous_credential(&anonymous_key, NOW - 10, NOW + 100, limits())
        .expect("anonymous credential");
    round_trip(&credential);
    let registration = create_anonymous_registration(
        credential,
        &anonymous_key,
        Some("turnstile".to_string()),
        "continuity".to_string(),
        "anonymous-op".to_string(),
        NOW,
        limits(),
    )
    .expect("anonymous registration");
    verify_anonymous_registration(&round_trip(&registration), NOW, limits())
        .expect("anonymous request verifies");
    round_trip(&RegisterAnonymousKeyResponse {
        anonymous_id: registration
            .credential
            .as_ref()
            .expect("credential")
            .anonymous_id
            .clone(),
        continuity_token: "receipt-only".to_string(),
        continuity_expires_at_unix_seconds: NOW + 100,
    });

    let capability = fixture.direct(&[SpoolCapabilityAction::Read]);
    let keyring = create_clone_keyring(
        fixture.spool_uuid,
        vec!["acme".to_string(), "heddle".to_string()],
        CloneOwnerPinKind::InvitationFingerprint,
        fixture.state.owner_id(),
        NOW,
        fixture.root.clone(),
        Vec::new(),
        vec![capability],
        NOW,
        limits(),
    )
    .expect("clone keyring");
    round_trip(keyring.pin.as_ref().expect("pin"));
    round_trip(&keyring);
    let temp = TempDir::new().expect("tempdir");
    let store = CloneKeyringStore::new(temp.path(), limits());
    store
        .install(keyring.clone(), NOW)
        .expect("keyring installs")
        .wire();
    assert!(matches!(
        store.install(keyring, NOW),
        Err(AuthorizationError::AlreadyPinned(_))
    ));
    store.load(NOW).expect("keyring reload verifies");
}

#[test]
fn deferred_bootstrap_objects_round_trip_and_verify() {
    let fixture = Fixture::new();
    let capability = fixture.direct(&[SpoolCapabilityAction::Read, SpoolCapabilityAction::Grant]);
    let bundle = fixture.bundle(capability);
    let origin = AuthorizationKey::from_seed([22; 32]).expect("origin key");
    let deferred_root =
        create_deferred_owner_root([23; 16], &origin, NOW + 100).expect("deferred root");
    let deferred = create_deferred_bootstrap(
        deferred_root,
        bundle,
        &origin,
        "deferred-bootstrap-op".to_string(),
        NOW,
        limits(),
    )
    .expect("deferred bootstrap");
    let deferred = round_trip(&deferred);
    let deferred_approval = match deferred.approval.as_ref() {
        Some(bootstrap_owner_root_request::Approval::DeferredHuman(approval)) => approval,
        _ => panic!("deferred bootstrap approval"),
    };
    round_trip(deferred_approval);
    verify_deferred_bootstrap(&deferred, NOW, limits())
        .expect("deferred bootstrap verifies after serialization");
    assert!(matches!(
        verify_deferred_bootstrap(&deferred, NOW + 101, limits()),
        Err(AuthorizationError::Invalid(_))
    ));
}
