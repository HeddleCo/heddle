// SPDX-License-Identifier: Apache-2.0
use crate::AuthToken;

#[test]
fn auth_token_creation_keeps_opaque_carrier_fields() {
    let token = AuthToken::new("token123", "credential-store");

    assert_eq!(token.id, "token123");
    assert_eq!(token.user, "credential-store");
}

#[test]
fn auth_token_serialization_round_trips_surviving_fields() {
    let token = AuthToken::new("token123", "env");

    let bytes = token.to_bytes();
    let restored = AuthToken::from_bytes(&bytes).unwrap();

    assert_eq!(restored, token);
}
