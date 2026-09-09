//! Native browser pairing retains the exact user-derived capability; the CLI
//! never turns approval into an independent credential-minting root.
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use chrono::Utc;
use crypto::{Ed25519Signer, Signer};
use prost::Message;
use thread_api::{contract as api, rpc};

use super::{
    agent_node_identity,
    auth::{AuthEvent, AuthLoginOutcome},
};

pub(super) async fn login(
    server: &str,
    open_browser: bool,
    on_event: &mut impl FnMut(AuthEvent) -> Result<()>,
) -> Result<AuthLoginOutcome> {
    let subject = Ed25519Signer::generate().context("generate paired credential key")?;
    let node = agent_node_identity::load_or_create()?;
    let endpoint = Ed25519Signer::from_seed(&node.secret_key().to_bytes())
        .context("load persistent endpoint signer")?;
    let client = super::auth::connect_enrolling_device_client(server, &subject).await?;
    let result = async {
        let remote = client.native().await?;
        let host = remote
            .description
            .endpoint
            .clone()
            .context("Weft descriptor has no endpoint")?;
        let operation = uuid::Uuid::new_v4().to_string();
        let begin = thread_api::pairing::sign_initiation(
            &subject,
            &endpoint,
            host,
            operation.clone(),
            Utc::now().timestamp(),
        )?;
        let response = remote
            .api
            .call::<rpc::IdentityServiceBeginPairing>(&begin)
            .await?;
        applied(response.receipt.as_ref(), &operation)?;
        let pending = response
            .pairing
            .context("pairing response omitted pending record")?;
        let reference = pending
            .r#ref
            .clone()
            .context("pairing response omitted reference")?;
        let pending_device = match pending.receiver.as_ref() {
            Some(api::pairing_record::Receiver::Device(device)) => device.clone(),
            _ => bail!("daemon pairing requires an actual device endpoint"),
        };
        if pending.subject_public_key != subject.public_key()
            || pending_device.public_key != endpoint.public_key()
            || pending.challenge.len() != 32
        {
            bail!("pairing response does not name the enrolling device keys");
        }
        super::auth::validate_browser_url(&response.verification_uri)?;
        on_event(AuthEvent::PairingReady {
            verification_uri: response.verification_uri.clone(),
        })?;
        if open_browser {
            on_event(AuthEvent::BrowserOpenRequested {
                url: response.verification_uri,
            })?;
        }
        on_event(AuthEvent::WaitingForAuthorization)?;
        let deadline = pending
            .expires_at
            .as_ref()
            .context("pairing has no deadline")?
            .seconds;
        let mut observation = remote
            .observe::<rpc::IdentityServiceObservePairing>(
                api::ObservePairingRequest {
                    pairing: Some(reference.clone()),
                    observe: Some(api::ObserveOptions {
                        mode: api::ObservationMode::Follow as i32,
                        ..Default::default()
                    }),
                },
                None,
            )
            .await?;
        let binding = loop {
            let remaining = deadline.saturating_sub(Utc::now().timestamp());
            if remaining <= 0 {
                bail!("pairing approval deadline expired; run `heddle auth login` again");
            }
            let batch = wait_for_pairing(
                std::time::Duration::from_secs(remaining as u64),
                observation.next_commit(),
            )
            .await?
            .context("pairing observation ended before approval")?;
            let mut approved = None;
            for change in batch.changes {
                if let api::pairing_event::Payload::Pairing(record) = change {
                    if record.r#ref.as_ref() != Some(&reference)
                        || record.subject_public_key != pending.subject_public_key
                        || record.receiver != pending.receiver
                        || record.challenge != pending.challenge
                    {
                        bail!("pairing observation changed the enrolled device");
                    }
                    if let Some(api::pairing_record::Approval::ApprovalBinding(binding)) =
                        record.approval
                    {
                        approved = Some(binding);
                    }
                }
            }
            if let Some(binding) = approved {
                break binding;
            }
        };
        drop(observation);
        if binding.subject_public_key != pending.subject_public_key
            || binding.device.as_ref() != Some(&pending_device)
            || binding.pairing_challenge != pending.challenge
        {
            bail!("approval commitment differs from pending pairing");
        }
        let attachment =
            thread_api::root_attachment::sign_binding(&subject, &endpoint, binding.clone())?;
        let operation = uuid::Uuid::new_v4().to_string();
        let response = remote
            .api
            .call::<rpc::IdentityServiceCompletePairing>(&api::CompletePairingRequest {
                client_operation_id: operation.clone(),
                pairing: Some(reference),
                proof: Some(api::complete_pairing_request::Proof::Attachment(
                    attachment.clone(),
                )),
            })
            .await?;
        finish(
            server,
            &subject,
            &binding,
            &attachment,
            response,
            &operation,
        )
    }
    .await;
    client.close().await;
    result
}

fn applied(receipt: Option<&api::MutationReceipt>, operation: &str) -> Result<()> {
    let receipt = receipt.context("pairing response omitted mutation receipt")?;
    if receipt.client_operation_id != operation
        || !matches!(
            receipt.outcome,
            Some(api::mutation_receipt::Outcome::Applied(_))
        )
    {
        bail!("pairing operation did not apply under the requested operation ID");
    }
    Ok(())
}
fn finish(
    server: &str,
    subject: &Ed25519Signer,
    binding: &api::RootAttachmentBinding,
    attachment: &api::RootAttachment,
    response: api::AuthenticationResponse,
    operation: &str,
) -> Result<AuthLoginOutcome> {
    let owner = response
        .ownership
        .clone()
        .context("pairing omitted account ownership")?;
    let credential = verify_response(subject, binding, attachment, response, operation)?;
    let now = Utc::now().timestamp();
    let authority = verify_pairing_authority(
        owner,
        binding,
        credential.mint_root_attachment.as_deref(),
        now,
    )?;
    repo::device_authority::publish(&repo::identity::heddle_home_dir(), &authority, now)?;
    let pem = credential
        .private_key_pem
        .as_deref()
        .context("verified pairing proof key missing")?;
    repo::identity::link_device_key(subject.public_key(), pem, server)
        .context("record paired local signing identity")?;
    let directory = repo::identity::heddle_home_dir().join("paired-devices");
    objects::fs_atomic::create_private_dir_all(&directory)?;
    let path = directory.join(format!("{}.pb", blake3::hash(server.as_bytes())));
    objects::fs_atomic::write_file_atomic_secret(&path, &attachment.encode_to_vec())?;
    let subject = credential.subject.clone();
    super::source_author::retain(server, &credential)?;
    config::credentials::store_server_credential(server, credential)?;
    Ok(AuthLoginOutcome::Authenticated {
        subject,
        credential_saved: true,
    })
}

fn verify_pairing_authority(
    owner: api::OwnerState,
    binding: &api::RootAttachmentBinding,
    mint_attachment: Option<&[u8]>,
    now: i64,
) -> Result<repo::device_authority::DeviceAuthority> {
    if owner
        .owner
        .as_ref()
        .is_none_or(|owner| owner.id != binding.account_id)
    {
        bail!("pairing ownership differs from the approving account");
    }
    let mint_roots = mint_attachment
        .map(api::SignedMintRootAttachment::decode)
        .transpose()
        .context("decode paired mint-root association")?
        .into_iter()
        .collect();
    let authority = repo::device_authority::DeviceAuthority {
        owner,
        mint_roots,
        revoked_ids: Vec::new(),
        revoked_mint_roots: Vec::new(),
        revoked_publishers: Vec::new(),
    };
    authority
        .verify_mint_root(&binding.root_public_key, now)
        .context("verify approved root belongs to the paired account")?;
    Ok(authority)
}

fn verify_response(
    subject: &Ed25519Signer,
    binding: &api::RootAttachmentBinding,
    attachment: &api::RootAttachment,
    response: api::AuthenticationResponse,
    operation: &str,
) -> Result<config::credentials::ServerCredential> {
    applied(response.receipt.as_ref(), operation)?;
    let principal = response.principal.context("pairing omitted account")?;
    if principal.account_id != binding.account_id || principal.id != binding.account_id {
        bail!("pairing completion changed approving account");
    }
    let result = response.credential.context("pairing omitted credential")?;
    let api::credential_result::Outcome::Issued(issued) = result
        .outcome
        .context("pairing omitted issued credential")?
    else {
        bail!("pairing cannot install independent mint authority");
    };
    if issued.proof_public_key != subject.public_key()
        || !matches!(
            api::CredentialKind::try_from(issued.kind),
            Ok(api::CredentialKind::Device | api::CredentialKind::Agent)
        )
    {
        bail!("paired credential changed subject or credential class");
    }
    let expiry = super::auth::credential_expiry(issued.expires_at.as_ref())?
        .context("paired credential omitted expiry")?;
    if expiry.timestamp() != binding.expires_at_unix_seconds {
        bail!("paired credential expiry differs from approved bound");
    }
    let root = biscuit_auth::PublicKey::from_bytes(
        &binding.root_public_key,
        biscuit_auth::Algorithm::Ed25519,
    )
    .context("approved credential root")?;
    // The account/root association was approved over this authenticated Weft
    // session; future direct calls use the stored user root and original chain.
    thread_api::root_attachment::verify(
        attachment,
        &issued.biscuit,
        &[root],
        &binding.account_id,
        binding
            .device
            .as_ref()
            .context("approved endpoint missing")?,
        Utc::now(),
    )?;
    let token = URL_SAFE.encode(&issued.biscuit);
    let facts = biscuit_verifier::verify_any_at_with_resource(
        &token,
        None,
        &[root],
        &[],
        "ObserveIdentity",
        None,
        Utc::now(),
    )
    .context("verify paired capability")?;
    if issued.subject != facts.sub {
        bail!("pairing response changed the verified credential subject");
    }
    let session = result.session.context("paired parent session missing")?;
    if session.revoked
        || session
            .r#ref
            .as_ref()
            .is_none_or(|reference| reference.spool.is_some() || reference.id != facts.sid)
    {
        bail!("pairing changed or revoked the original parent session");
    }
    let credential_id = issued
        .r#ref
        .filter(|reference| reference.spool.is_none() && reference.id.starts_with("paired:"))
        .context("paired credential reference missing")?
        .id;
    let pem = subject.to_pem().context("encode paired proof key")?;
    Ok(config::credentials::ServerCredential {
        mint_root_attachment: result
            .mint_root_attachment
            .map(|proof| proof.encode_to_vec()),
        token,
        subject: facts.sub,
        device_id: Some(credential_id.clone()),
        credential_id: Some(credential_id),
        private_key_pem: Some(pem),
        expires_at: Some(expiry.to_rfc3339()),
    })
}

async fn wait_for_pairing<T, E: std::fmt::Display>(
    remaining: std::time::Duration,
    future: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    tokio::time::timeout(remaining, future)
        .await
        .context("pairing approval deadline expired; run `heddle auth login` again")?
        .map_err(|error| anyhow::anyhow!("pairing observation failed: {error}"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pairing_requires_verified_same_account_current_authority() {
        let root = Ed25519Signer::from_seed(&[71; 32]).expect("root");
        let recovery = Ed25519Signer::from_seed(&[72; 32]).expect("recovery");
        let signed = repo::sign_custodial_owner_root(&root, &recovery, [9; 16], [5; 32])
            .expect("root proof");
        let signed_binding =
            repo::sign_custodial_owner_binding(&root, &signed, [6; 32]).expect("binding");
        let verified =
            heddleco_capability_verifier::verify_owner_root(&signed).expect("verify root");
        let owner = api::OwnerState {
            owner: Some(api::PrincipalRef {
                id: uuid::Uuid::from_bytes([9; 16]).to_string(),
            }),
            root: Some(signed),
            binding: Some(signed_binding),
            version: verified.state_hash().to_vec(),
            ..Default::default()
        };
        let mut binding = api::RootAttachmentBinding {
            account_id: uuid::Uuid::from_bytes([9; 16]).to_string(),
            root_public_key: root.public_key().to_vec(),
            ..Default::default()
        };
        verify_pairing_authority(owner.clone(), &binding, None, 100).expect("matching authority");
        binding.account_id = uuid::Uuid::from_bytes([8; 16]).to_string();
        assert!(
            verify_pairing_authority(owner.clone(), &binding, None, 100)
                .err()
                .expect("wrong account")
                .to_string()
                .contains("approving account")
        );
        binding.account_id = uuid::Uuid::from_bytes([9; 16]).to_string();
        binding.root_public_key = vec![99; 32];
        assert!(verify_pairing_authority(owner.clone(), &binding, None, 100).is_err());
        binding.root_public_key = root.public_key().to_vec();
        let mut changed = owner;
        changed.version = vec![0; 32];
        assert!(verify_pairing_authority(changed, &binding, None, 100).is_err());
    }

    #[tokio::test]
    async fn stalled_pairing_observation_respects_ceremony_deadline() {
        let error = wait_for_pairing(
            std::time::Duration::from_millis(1),
            std::future::pending::<std::result::Result<(), std::io::Error>>(),
        )
        .await
        .expect_err("stalled stream expires");
        assert!(
            error
                .to_string()
                .contains("pairing approval deadline expired")
        );
    }
    #[test]
    fn paired_response_keeps_original_credential_and_rejects_changed_key_or_session() {
        let root = biscuit_auth::KeyPair::new();
        let subject = Ed25519Signer::from_seed(&[61; 32]).expect("subject");
        let endpoint = Ed25519Signer::from_seed(&[62; 32]).expect("endpoint");
        let account = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().timestamp();
        let expiry = chrono::DateTime::from_timestamp(now + 300, 0).expect("expiry");
        let token = biscuit_auth::Biscuit::builder()
            .code(format!("user(\"{account}\"); session(\"parent-session\"); device_pop_key(\"{}\"); check if time($now), $now < {};", hex::encode(subject.public_key()), expiry.to_rfc3339()).as_str())
            .expect("fixture facts").build(&root).expect("credential").to_vec().expect("bytes");
        let binding = api::RootAttachmentBinding {
            format_version: 2,
            account_id: account.clone(),
            root_public_key: root.public().to_bytes(),
            subject_public_key: subject.public_key().to_vec(),
            device: Some(api::EndpointRef {
                public_key: endpoint.public_key().to_vec(),
                kind: api::EndpointKind::Device as i32,
            }),
            not_before_unix_seconds: now,
            expires_at_unix_seconds: now + 300,
            credential_digest: blake3::hash(&token).as_bytes().to_vec(),
            pairing_challenge: vec![4; 32],
        };
        let attachment =
            thread_api::root_attachment::sign_binding(&subject, &endpoint, binding.clone())
                .expect("attachment");
        let response = api::AuthenticationResponse {
            receipt: Some(api::MutationReceipt {
                client_operation_id: "pairing-op".into(),
                outcome: Some(api::mutation_receipt::Outcome::Applied(
                    api::Applied::default(),
                )),
                ..Default::default()
            }),
            principal: Some(api::PrincipalRecord {
                id: account.clone(),
                account_id: account,
                ..Default::default()
            }),
            credential: Some(api::CredentialResult {
                outcome: Some(api::credential_result::Outcome::Issued(
                    api::IssuedCredential {
                        r#ref: Some(api::RecordRef {
                            id: format!("paired:{}", uuid::Uuid::new_v4()),
                            spool: None,
                        }),
                        biscuit: token.clone(),
                        subject: binding.account_id.clone(),
                        proof_public_key: subject.public_key().to_vec(),
                        kind: api::CredentialKind::Device as i32,
                        expires_at: Some(prost_types::Timestamp {
                            seconds: now + 300,
                            nanos: 0,
                        }),
                        ..Default::default()
                    },
                )),
                session: Some(api::SessionRecord {
                    r#ref: Some(api::RecordRef {
                        id: "parent-session".into(),
                        spool: None,
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let verified = verify_response(
            &subject,
            &binding,
            &attachment,
            response.clone(),
            "pairing-op",
        )
        .expect("exact response");
        assert_eq!(
            URL_SAFE.decode(verified.token).expect("stored bytes"),
            token
        );
        let mut changed = response.clone();
        let Some(api::credential_result::Outcome::Issued(issued)) = changed
            .credential
            .as_mut()
            .expect("credential")
            .outcome
            .as_mut()
        else {
            panic!("issued")
        };
        issued.proof_public_key = endpoint.public_key().to_vec();
        assert!(
            verify_response(&subject, &binding, &attachment, changed, "pairing-op")
                .expect_err("changed proof key")
                .to_string()
                .contains("changed subject")
        );
        let mut changed = response.clone();
        let Some(api::credential_result::Outcome::Issued(issued)) = changed
            .credential
            .as_mut()
            .expect("credential")
            .outcome
            .as_mut()
        else {
            panic!("issued credential")
        };
        issued.subject = "unrelated-subject".into();
        assert!(
            verify_response(&subject, &binding, &attachment, changed, "pairing-op")
                .expect_err("response subject must match verified Biscuit")
                .to_string()
                .contains("credential subject")
        );
        let mut changed = response;
        changed
            .credential
            .as_mut()
            .expect("credential")
            .session
            .as_mut()
            .expect("session")
            .r#ref
            .as_mut()
            .expect("ref")
            .id = "different-session".into();
        assert!(
            verify_response(&subject, &binding, &attachment, changed, "pairing-op")
                .expect_err("changed parent session")
                .to_string()
                .contains("original parent session")
        );
    }
}
