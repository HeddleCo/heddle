//! Network drain for durable local timeline requests. Hooks never call this.
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use api::heddle::api::v1alpha2::{
    RegisterTimelineOriginRequest, upload_scrubbed_timeline_response::Outcome,
};
use config::UserConfig;
use crypto::{Ed25519Signer, Signer};
use repo::{device_catalog::store::Catalog, device_runs::RunStore};

use super::{
    HostedAuthMode, HostedClient, HostedSession, TimelineUploadFailureKind,
    canonical_server_authority, descriptor_trust,
};

pub async fn drain_timeline_outbox_once() -> Result<()> {
    let home = repo::identity::heddle_home_dir();
    let Some(catalog) = Catalog::read(&home)? else {
        return Ok(());
    };
    let mut after = String::new();
    loop {
        let page = catalog.spools(&after, 128, 4 * 1024 * 1024)?;
        for spool in &page.records {
            let Some(store) = RunStore::open_existing(&spool.registration.heddle_dir)? else {
                continue;
            };
            for run in store.incomplete_timeline_runs(8)? {
                store.repair_timeline_upload(&run)?;
            }
            let now = chrono::Utc::now().timestamp_millis();
            let registration = store.next_timeline_registration(now)?;
            let upload = store.next_timeline_upload(now)?;
            if registration.is_none() && upload.is_none() {
                continue;
            }
            if let Some(device) =
                repo::identity::load_device(&repo::identity::device_identity_path())?
            {
                require_retained_bearer(&store, &device)?;
            }
            let (client, target_key) =
                match tokio::time::timeout(Duration::from_secs(10), uploader_client()).await {
                    Ok(Ok(value)) => value,
                    Ok(Err(error)) => {
                        if let Some(pending) = &registration {
                            store.retry_timeline_registration(
                                &pending.request.client_operation_id,
                                now,
                            )?;
                        }
                        if let Some(pending) = &upload {
                            store
                                .retry_timeline_upload(&pending.request.client_operation_id, now)?;
                        }
                        return Err(error);
                    }
                    Err(_) => {
                        if let Some(pending) = &registration {
                            store.retry_timeline_registration(
                                &pending.request.client_operation_id,
                                now,
                            )?;
                        }
                        if let Some(pending) = &upload {
                            store
                                .retry_timeline_upload(&pending.request.client_operation_id, now)?;
                        }
                        return Ok(());
                    }
                };
            if let Some(registration) = registration {
                ensure!(
                    registration.target_deployment == target_key,
                    "timeline registration targets another deployment"
                );
                process_registration(&store, &client, &registration.request, now).await?;
            }
            if let Some(upload) = upload {
                ensure!(
                    upload.target_deployment == target_key,
                    "timeline upload targets another deployment"
                );
                let operation = upload.request.client_operation_id.clone();
                let run = upload.request.run.as_ref().map(|r| r.id.clone());
                let response = tokio::time::timeout(
                    Duration::from_secs(10),
                    client.upload_scrubbed_timeline(&upload.request),
                )
                .await
                .unwrap_or(Err(TimelineUploadFailureKind::Retry));
                match response {
                    Ok(response) => match response.outcome {
                        Some(Outcome::Ack(ack)) => {
                            store.acknowledge_timeline_upload(&operation, &ack)?;
                            if let Some(run) = run {
                                store.repair_timeline_upload(&run)?;
                            }
                        }
                        Some(Outcome::Gap(gap)) => {
                            store.repair_timeline_gap(&operation, gap.expected_next_position)?;
                        }
                        None => store.deny_timeline_upload(&operation, "upload_denied")?,
                    },
                    Err(TimelineUploadFailureKind::Retry) => {
                        store.retry_timeline_upload(&operation, now)?;
                    }
                    Err(TimelineUploadFailureKind::Gone) => {
                        store.deny_timeline_upload(&operation, "resource_gone")?;
                    }
                    Err(TimelineUploadFailureKind::Conflict) => {
                        store.deny_timeline_upload(&operation, "conflict")?;
                    }
                    Err(TimelineUploadFailureKind::Denied) => {
                        store.deny_timeline_upload(&operation, "upload_denied")?;
                    }
                }
            }
            client.close().await;
            return Ok(());
        }
        if !page.has_more {
            return Ok(());
        }
        after = page
            .records
            .last()
            .context("catalog page made no progress")?
            .registration
            .id
            .to_string();
    }
}

fn require_retained_bearer(
    store: &RunStore,
    device: &repo::identity::DeviceIdentity,
) -> Result<()> {
    if device.credential_token.is_none() || device.credential_subject.is_none() {
        store.require_timeline_reenrollment()?;
        anyhow::bail!("timeline uploader: re-enroll this device");
    }
    Ok(())
}

async fn process_registration(
    store: &RunStore,
    client: &HostedClient,
    request: &RegisterTimelineOriginRequest,
    now: i64,
) -> Result<()> {
    let operation = &request.client_operation_id;
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client.register_timeline_origin(request),
    )
    .await
    .unwrap_or(Err(TimelineUploadFailureKind::Retry));
    match response {
        Ok(response) => {
            let origin = request
                .origin
                .as_ref()
                .context("stored registration origin missing")?;
            ensure!(
                response.origin_sha256 == api::timeline_upload::origin_digest(origin)?.to_vec()
                    && response.registered_at.is_some(),
                "timeline registration response differs from stored origin"
            );
            store.register_timeline_origin(operation)?;
        }
        Err(TimelineUploadFailureKind::Retry) => {
            store.retry_timeline_registration(operation, now)?;
        }
        Err(_) => {
            store.deny_timeline_registration(operation)?;
        }
    }
    Ok(())
}

async fn uploader_client() -> Result<(HostedClient, Vec<u8>)> {
    let device = repo::identity::load_device(&repo::identity::device_identity_path())?
        .context("timeline uploader has no enrolled device key")?;
    let canonical_server = canonical_server_authority(&device.server)?;
    let target_key = descriptor_trust::load_automatic_pin(&canonical_server)?
        .context("timeline deployment identity pin unavailable")?
        .public_key_bytes()?
        .to_vec();
    let bearer = validated_device_bearer(&device)?;
    let session = HostedSession::build(
        &UserConfig::load_default()?,
        Some(device.server.clone()),
        bearer,
    )?;
    let client = session
        .connect_outbound(&device.server)
        .await
        .map_err(|error| anyhow::anyhow!("connecting timeline uploader: {error}"))?;
    Ok((client, target_key))
}

fn validated_device_bearer(device: &repo::identity::DeviceIdentity) -> Result<HostedAuthMode> {
    let device_signer = Ed25519Signer::from_pem(&device.private_key_pem)?;
    ensure!(
        hex::encode(device_signer.public_key()) == device.public_key,
        "timeline uploader key differs from enrolled device"
    );
    let token = device
        .credential_token
        .as_deref()
        .context("timeline uploader has no retained enrolled bearer")?;
    let subject = device
        .credential_subject
        .as_deref()
        .context("timeline uploader retained bearer has no subject")?;
    ensure!(
        crate::hosted_runtime::device_flow::authenticated_subject(token)? == subject,
        "timeline uploader bearer subject differs from its credential"
    );
    ensure!(
        crate::hosted_runtime::device_flow::effective_pop_public_key_hex(token)?
            .eq_ignore_ascii_case(&device.public_key),
        "timeline uploader bearer proof key differs from enrolled device"
    );
    Ok(HostedAuthMode::PresentedDevice {
        token: token.to_owned(),
        proof_key_pem: device.private_key_pem.clone(),
        subject: subject.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploader_presents_the_exact_retained_device_bearer() {
        let signer = Ed25519Signer::from_seed(&[23; 32]).expect("device key");
        let root = crate::hosted_runtime::root_mint::mint_independent_root(
            crate::hosted_runtime::root_mint::IndependentRootMint {
                seed: &signer.to_seed(),
                subject: "device@example.test",
                ttl: crate::hosted_runtime::root_mint::ACCOUNT_ROOT_TTL,
                credential_id: Some("device-credential"),
                session_id: None,
                expires_at: None,
            },
        )
        .expect("persisted bearer");
        let device = repo::identity::DeviceIdentity {
            public_key: hex::encode(signer.public_key()),
            private_key_pem: root.private_key_pem.clone(),
            server: "api.example.test".into(),
            linked_at: String::new(),
            credential_token: Some(root.token.clone()),
            credential_subject: Some(root.subject.clone()),
        };
        let bearer = validated_device_bearer(&device).expect("device bearer");
        let HostedAuthMode::PresentedDevice { token, .. } = bearer else {
            panic!("expected stored device bearer");
        };
        assert!(token == root.token);
        let other = Ed25519Signer::from_seed(&[24; 32]).expect("other key");
        let other_root = crate::hosted_runtime::root_mint::mint_independent_root(
            crate::hosted_runtime::root_mint::IndependentRootMint {
                seed: &other.to_seed(),
                subject: "device@example.test",
                ttl: crate::hosted_runtime::root_mint::ACCOUNT_ROOT_TTL,
                credential_id: None,
                session_id: None,
                expires_at: None,
            },
        )
        .expect("other bearer");
        let mut substituted = device;
        substituted.credential_token = Some(other_root.token);
        assert!(validated_device_bearer(&substituted).is_err());
    }

    #[test]
    fn older_enrolled_identity_requires_reenrollment_in_upload_health() {
        let directory = tempfile::tempdir().expect("directory");
        let store = RunStore::open(directory.path()).expect("store");
        let connection = repo::local_metadata::open(directory.path()).expect("database");
        connection.execute("INSERT INTO timeline_upload_runs(run,origin,origin_biscuit,target_deployment,registration_operation_id) VALUES('run-1',X'01',X'',X'02','registration')", []).expect("pending registration");
        let signer = Ed25519Signer::from_seed(&[23; 32]).expect("device key");
        let device = repo::identity::DeviceIdentity {
            public_key: hex::encode(signer.public_key()),
            private_key_pem: signer.to_pem().expect("pem"),
            server: "api.example.test".into(),
            linked_at: String::new(),
            credential_token: None,
            credential_subject: None,
        };
        assert!(
            require_retained_bearer(&store, &device)
                .expect_err("older device must be visible")
                .to_string()
                .contains("re-enroll this device")
        );
        assert_eq!(
            store.upload_health().expect("health").incomplete_reasons,
            ["re-enroll this device"]
        );
        assert!(validated_device_bearer(&device).is_err());
    }
}
