//! Locally verified identity projections. Hosted-only administration is explicit
//! unavailable coverage; reading this device never contacts the hosted account.
use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1::*;

use super::{DeviceRpc, account_auth::AccountSession};

impl DeviceRpc {
    pub(super) fn identity_snapshot(
        &self,
        session: &AccountSession,
        query: &ObserveIdentityRequest,
    ) -> Result<Vec<(String, IdentityEvent)>> {
        let now = chrono::Utc::now().timestamp();
        let authority = session.owner(&self.home)?;
        let owner = repo::verify_account_owner_observation(&authority.owner, now)?;
        let facts = session.view_facts()?;
        let mut tier = match authority.owner.binding.as_ref().map(|b| b.kind) {
            Some(kind) if kind == OwnerKeyBindingKind::SelfRootedRegistration as i32 => {
                RootingTier::SelfRooted
            }
            Some(kind) if kind == OwnerKeyBindingKind::ServerRootedCustody as i32 => {
                RootingTier::ServerRooted
            }
            Some(kind) if kind == OwnerKeyBindingKind::AgentClaim as i32 => {
                RootingTier::AgentRooted
            }
            _ => RootingTier::Unspecified,
        };
        if authority.owner.accepted_transitions.iter().any(|signed| {
            signed.transition.as_ref().is_some_and(|transition| {
                transition.kind == OwnerKeyTransitionKind::ClaimDeferredHuman as i32
            })
        }) {
            tier = RootingTier::SelfRooted;
        }
        let principal = PrincipalRecord {
            id: session.principal.clone(),
            account_id: session.principal.clone(),
            version: authority.owner.version.clone(),
            root_public_key: owner.authority_key().public_key.clone(),
            acting_agent_id: facts.delegation_agent_id.clone().unwrap_or_default(),
            rooting_tier: tier as i32,
            ..Default::default()
        };
        let mut rows = vec![(
            "principal".into(),
            IdentityEvent {
                payload: Some(identity_event::Payload::Identity(principal)),
                ..Default::default()
            },
        )];
        if let Some(page) = &query.devices {
            if !page.after_page.is_empty() {
                bail!("local device page cursor is invalid");
            }
            rows.push((
                "device".into(),
                IdentityEvent {
                    payload: Some(identity_event::Payload::Device(DeviceIdentity {
                        endpoint: Some(self.endpoint()),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            ));
            rows.push(status("devices", Coverage::Complete));
        }
        for (section, requested) in [
            ("sessions", query.sessions.is_some()),
            ("signup_invitations", query.signup_invitations.is_some()),
            ("delegations", query.delegations.is_some()),
            ("recovery", query.recovery.is_some()),
            ("billing", query.include_billing),
        ] {
            rows.push(status(
                section,
                if requested {
                    Coverage::Unavailable
                } else {
                    Coverage::NotRequested
                },
            ));
        }
        if query.include_current_credential {
            let proof = repo::thread_replication::metadata::prepare_control_authority(
                &authority,
                &session
                    .root
                    .to_bytes()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("mint root key must contain 32 bytes"))?,
                &session.token,
                now,
            )?;
            let version = blake3::hash(
                &[
                    authority.owner.version.as_slice(),
                    proof.as_slice(),
                    session.publisher.as_slice(),
                ]
                .concat(),
            )
            .as_bytes()
            .to_vec();
            rows.push((
                "current_credential".into(),
                IdentityEvent {
                    payload: Some(identity_event::Payload::CurrentCredential(
                        CurrentCredentialRecord {
                            version,
                            kind: if facts.delegation_agent_id.is_some() {
                                CredentialKind::Agent
                            } else {
                                CredentialKind::Device
                            } as i32,
                            subject: facts.sub.clone(),
                            proof_public_key: session.publisher.to_vec(),
                            acting_agent_id: facts.delegation_agent_id.unwrap_or_default(),
                            expires_at: (session.expires != 0).then_some(prost_types::Timestamp {
                                seconds: session.expires,
                                nanos: 0,
                            }),
                            thread_control_authority: proof,
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                },
            ));
            rows.push(status("current_credential", Coverage::Complete));
        }
        Ok(rows)
    }
    pub(super) fn ownership_snapshot(
        &self,
        session: &AccountSession,
        query: &ObserveOwnershipRequest,
    ) -> Result<Vec<(String, OwnershipEvent)>> {
        if query
            .owner
            .as_ref()
            .is_some_and(|owner| owner.id != session.principal)
        {
            bail!("owner differs from this admitted account");
        }
        let mut owner = session.owner(&self.home)?.owner;
        // Hosted pending ceremonies and resource authorization are not advanced by
        // this offline view. Only the cryptographically accepted history is live.
        owner.pending_transitions.clear();
        owner.resource_keyring = None;
        let mut rows = vec![(
            "owner".into(),
            OwnershipEvent {
                payload: Some(ownership_event::Payload::Owner(owner)),
                ..Default::default()
            },
        )];
        if query.spool.is_some() {
            rows.push((
                "resource_lineage".into(),
                OwnershipEvent {
                    payload: Some(ownership_event::Payload::Status(SectionStatus {
                        section: "resource_lineage".into(),
                        coverage: Coverage::Unavailable as i32,
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            ));
        }
        Ok(rows)
    }
    pub(super) fn introspect(
        &self,
        session: &AccountSession,
        query: &IntrospectCredentialRequest,
    ) -> Result<CredentialInspection> {
        if query.biscuit.is_empty() || query.biscuit.len() > 65536 {
            bail!("bounded Biscuit required");
        }
        let encoded = std::str::from_utf8(&query.biscuit).context("Biscuit must be base64")?;
        let root = biscuit_verifier::unverified_authority_device_pop_key(encoded)?
            .context("credential mint root missing")?;
        let authority = session.owner(&self.home)?;
        let now = chrono::Utc::now().timestamp();
        let current = repo::verify_account_owner_observation(&authority.owner, now)?;
        let recognized = current.authority_key().public_key == root.to_bytes()
            || current
                .signed_root()
                .root
                .as_ref()
                .and_then(|root| root.authority_key.as_ref())
                .is_some_and(|key| key.public_key == root.to_bytes())
            || authority.owner.accepted_transitions.iter().any(|signed| {
                signed
                    .transition
                    .as_ref()
                    .and_then(|transition| transition.next_authority_key.as_ref())
                    .is_some_and(|key| key.public_key == root.to_bytes())
            })
            || authority.mint_roots.iter().any(|signed| {
                signed
                    .attachment
                    .as_ref()
                    .and_then(|attachment| attachment.mint_root_key.as_ref())
                    .is_some_and(|key| key.public_key == root.to_bytes())
            });
        if !recognized {
            bail!("credential belongs to an unrecognized authority");
        }
        let parsed = biscuit_verifier::parse_token(encoded, &[root])?;
        let inspected = biscuit_verifier::inspect_verified_credential(&parsed, &root)?;
        if inspected
            .asserted_account
            .is_some_and(|id| id.to_string() != session.principal)
        {
            bail!("credential belongs to another account");
        }
        let publisher: [u8; 32] = inspected
            .proof_public_key
            .as_slice()
            .try_into()
            .context("proof key")?;
        let revoked = authority.verify_mint_root(&root.to_bytes(), now).is_err()
            || authority.verify_publisher(&publisher).is_err()
            || inspected
                .revocation_ids
                .iter()
                .any(|id| authority.revoked_ids.contains(id));
        Ok(CredentialInspection {
            authority: Some(RootAttachment {
                root_public_key: root.to_bytes(),
                subject_public_key: publisher.to_vec(),
                ..Default::default()
            }),
            expires_at: if inspected.expires_at_unix_seconds == 0 {
                None
            } else {
                Some(prost_types::Timestamp {
                    seconds: i64::try_from(inspected.expires_at_unix_seconds)?,
                    nanos: 0,
                })
            },
            revoked,
            actions: vec![],
        })
    }
}
fn status(section: &str, coverage: Coverage) -> (String, IdentityEvent) {
    (
        format!("status:{section}"),
        IdentityEvent {
            payload: Some(identity_event::Payload::Status(SectionStatus {
                section: section.into(),
                coverage: coverage as i32,
                ..Default::default()
            })),
            ..Default::default()
        },
    )
}
