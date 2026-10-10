// SPDX-License-Identifier: Apache-2.0
#[path = "../../tests/gateway_publication/support.rs"]
mod support;
use super::*;
use heddle_git_projection::gateway_publication::{HistoryBudget, PreparedHistory};
use objects::{object::original_boundary_acceptance::BoundaryOriginalKind, store::ObjectStore};

fn scope(f: &support::Fixture) -> GitTransportScope {
    GitTransportScope {
        service_audience: "git-gateway".into(),
        tenant_spool: f.reference.spool.clone(),
        spool: f.reference.spool.clone(),
        repository_path: "fixture/repository".into(),
        thread: f.reference.id.clone(),
        action: GitTransportAction::Read as i32,
        disclosure_audience: "public".into(),
    }
}
fn plan(f: &support::Fixture) -> PreparedHistory {
    let mut plan = PreparedHistory::prepare(
        &support::remote(),
        &f.store,
        f.selection(),
        f.scope(),
        f.root.path(),
        HistoryBudget::default(),
        |_| Ok(()),
    )
    .expect("actual native history");
    let signer = crypto::Ed25519Signer::from_seed(&[62; 32]).expect("fixture signer");
    for revision in plan.revisions_mut() {
        revision
            .sign_acceptance(
                support::author(),
                [BoundaryOriginalKind::Source].into(),
                &signer,
            )
            .expect("boundary proofs");
    }
    plan
}
#[tokio::test]
async fn native_bootstrap_roundtrips_exact_source_through_token_free_proof() {
    let f = support::fixture(200);
    let plan = plan(&f);
    let staged = StagedPublication::stage_bootstrap(
        f.root.path(),
        &plan,
        "00000000-0000-0000-0000-0000000001f6",
        &scope(&f),
    )
    .await
    .expect("stage");
    assert!(staged.is_bootstrap());
    assert!(staged.accepted_receipt().is_err());
    assert!(staged.submit(&support::remote(), "not-used").await.is_err());
    let proof = staged.export_proof().expect("proof");
    let artifacts = staged.export_artifacts().expect("native artifacts");
    assert!(!proof.windows(6).any(|w| w == b"ggit1_"));
    let destination = tempfile::tempdir().expect("new process disk");
    let restored =
        StagedPublication::import(destination.path(), &proof, &artifacts).expect("restore");
    assert_eq!(staged.operation(), restored.operation());
    assert_eq!(
        staged.client_operation_id().expect("op"),
        restored.client_operation_id().expect("restored op")
    );
    assert_eq!(restored.scope().expect("scope"), scope(&f));
    let hydrated = restored
        .hydrate(f.spool_genesis())
        .expect("genuine pack validation");
    assert_eq!(hydrated.tip, plan.tip());
    for blob in &f.blobs {
        assert_eq!(
            hydrated
                .source
                .get_blob(&blob.hash())
                .expect("lookup")
                .expect("historical bytes")
                .content(),
            blob.content()
        );
    }
    assert!(
        hydrated.project().is_ok(),
        "shared exact fixed-recipe projection"
    );
    let repeated = StagedPublication::import(destination.path(), &proof, &artifacts)
        .expect("idempotent restore");
    assert_eq!(repeated.operation(), restored.operation());
}
#[tokio::test]
async fn staging_refuses_corrupt_missing_and_extra_native_bytes() {
    let f = support::fixture(70);
    let plan = plan(&f);
    let staged = StagedPublication::stage_bootstrap(
        f.root.path(),
        &plan,
        "00000000-0000-0000-0000-0000000001f6",
        &scope(&f),
    )
    .await
    .expect("stage");
    let proof = staged.export_proof().expect("proof");
    let mut artifacts = staged.export_artifacts().expect("native artifacts");
    let target = tempfile::tempdir().expect("restore disk");
    assert!(StagedPublication::import(target.path(), &proof, &artifacts[1..]).is_err());
    artifacts[0].bytes[0] ^= 1;
    assert!(StagedPublication::import(target.path(), &proof, &artifacts).is_err());
    artifacts[0].bytes[0] ^= 1;
    artifacts.push(artifacts[0].clone());
    assert!(StagedPublication::import(target.path(), &proof, &artifacts).is_err());
}
#[tokio::test]
async fn staging_refuses_tar_alias_and_extra_metadata() {
    let f = support::fixture(20);
    let plan = plan(&f);
    let staged = StagedPublication::stage_bootstrap(
        f.root.path(),
        &plan,
        "00000000-0000-0000-0000-0000000001f6",
        &scope(&f),
    )
    .await
    .expect("stage");
    let proof = staged.export_proof().expect("proof");
    let mut trailing_secret = proof.clone();
    trailing_secret.extend_from_slice(b"ggit1_hidden-secret-after-tar-end");
    assert!(StagedPublication::inspect_proof(&trailing_secret).is_err());
    let artifacts = staged.export_artifacts().expect("artifacts");
    let mut archive = tar::Builder::new(Vec::new());
    for entry in tar::Archive::new(proof.as_slice())
        .entries()
        .expect("entries")
    {
        let mut entry = entry.expect("entry");
        let header = entry.header().clone();
        archive.append(&header, &mut entry).expect("retain exact");
    }
    let mut header = tar::Header::new_gnu();
    header.set_size(6);
    header.set_mode(0o600);
    header.set_cksum();
    archive
        .append_data(&mut header, "session", b"secret".as_slice())
        .expect("extra");
    let malicious = archive.into_inner().expect("archive");
    let target = tempfile::tempdir().expect("restore disk");
    assert!(StagedPublication::import(target.path(), &malicious, &artifacts).is_err());
}

#[test]
fn complete_originals_digest_verifies_signatures_and_ignores_only_identical_repetitions() {
    let f = support::fixture(20);
    let plan = plan(&f);
    let originals: Vec<_> = plan
        .revisions()
        .iter()
        .map(|r| r.publication().originals().clone())
        .collect();
    let digest =
        thread_api::publication::git_originals_digest(&originals).expect("verified originals");
    assert_eq!(
        digest,
        thread_api::publication::git_originals_digest(originals.iter().rev())
            .expect("order independent")
    );
    assert_eq!(
        digest,
        thread_api::publication::git_originals_digest(originals.iter().chain(&originals))
            .expect("identical repetition")
    );
    let mut changed = originals.clone();
    changed[0].operations[0].operations[0].signatures[0].signature[0] ^= 1;
    assert!(thread_api::publication::git_originals_digest(&changed).is_err());
    let removed = originals[0].operations[0].operations[0]
        .canonical_record
        .clone();
    let mut incomplete = originals;
    for revision in &mut incomplete {
        for batch in &mut revision.operations {
            batch
                .operations
                .retain(|record| record.canonical_record != removed);
        }
    }
    assert_ne!(
        digest,
        thread_api::publication::git_originals_digest(&incomplete)
            .expect("remaining verified originals")
    );
}
