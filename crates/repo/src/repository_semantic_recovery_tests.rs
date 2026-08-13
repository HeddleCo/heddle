// SPDX-License-Identifier: Apache-2.0

use std::{fs, path::Path};

use objects::{
    object::{Attribution, Blob, Principal, State, StateId, ThreadName, Tree, TreeEntry},
    store::ObjectStore,
};
use semantic_recovery::{Embedder, ModelIdentity, RecoveryError};

use super::Repository;

struct FixtureEmbedder;

impl Embedder for FixtureEmbedder {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            id: "fixture-local-model".to_string(),
            artifact_sha256: "fixture".to_string(),
            dimensions: 4,
        }
    }

    fn embed(&mut self, texts: &[String]) -> std::result::Result<Vec<Vec<f32>>, RecoveryError> {
        Ok(texts
            .iter()
            .map(|text| {
                if text.contains("invoice") {
                    vec![1.0, 0.01, 0.0, 0.0]
                } else if text.contains("dns") {
                    vec![0.01, 1.0, 0.0, 0.0]
                } else {
                    vec![0.0, 0.0, 1.0, 0.01]
                }
            })
            .collect())
    }
}

#[test]
fn sidecar_rebuild_and_loss_leave_canonical_objects_byte_identical() {
    let temporary = tempfile::tempdir().unwrap();
    let repo = Repository::init_default(temporary.path()).unwrap();
    let root = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .unwrap();

    let alpha_one = put_state(
        &repo,
        root,
        "invoice proration preserves currency precision",
        "pub fn prorate_invoice(total: Money) -> Money { total.round_currency() }",
    );
    let alpha_two = put_state(
        &repo,
        alpha_one,
        "invoice proration after billing rename",
        "pub fn allocate_charge(amount: Money) -> Money { amount.round_currency() }",
    );
    let beta_one = put_state(
        &repo,
        root,
        "dns cache respects stale while revalidate",
        "pub fn refresh_dns(record: DnsRecord) { record.keep_stale_ttl(); }",
    );
    let beta_two = put_state(
        &repo,
        beta_one,
        "dns cache after resolver reorder",
        "fn validate_ttl() {}\npub fn lookup_dns() { validate_ttl(); }",
    );
    repo.refs()
        .set_thread(&ThreadName::new("feature/invoice"), &alpha_two)
        .unwrap();
    repo.refs()
        .set_thread(&ThreadName::new("feature/dns"), &beta_two)
        .unwrap();

    let objects = repo.heddle_dir().join("objects");
    let before = directory_snapshot(&objects);
    let report = repo
        .rebuild_semantic_recovery_index(&mut FixtureEmbedder)
        .unwrap();
    assert_eq!(report.states, 5);
    assert_eq!(report.threads, 3);
    assert_eq!(report.packed_bits_per_vector, 9);
    assert!(report.path.starts_with(repo.heddle_dir().join("indexes")));
    assert_eq!(before, directory_snapshot(&objects));

    let recovered = repo
        .reconstruct_semantic_thread(&alpha_two, &mut FixtureEmbedder, 4)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.thread, "feature/invoice");
    assert_eq!(recovered.siblings[0].state, alpha_one);

    fs::remove_file(repo.semantic_recovery_index_path()).unwrap();
    assert!(
        repo.reconstruct_semantic_thread(&alpha_two, &mut FixtureEmbedder, 4)
            .unwrap()
            .is_none()
    );
    assert_eq!(before, directory_snapshot(&objects));
    assert!(repo.store().get_state(&alpha_two).unwrap().is_some());

    let rebuilt = repo
        .rebuild_semantic_recovery_index(&mut FixtureEmbedder)
        .unwrap();
    assert_eq!(rebuilt.states, report.states);
    assert_eq!(before, directory_snapshot(&objects));
    assert_eq!(
        repo.reconstruct_semantic_thread(&alpha_two, &mut FixtureEmbedder, 4)
            .unwrap()
            .unwrap()
            .thread,
        "feature/invoice"
    );
}

fn put_state(repo: &Repository, parent: StateId, intent: &str, content: &str) -> StateId {
    let blob = repo.store().put_blob(&Blob::from(content)).unwrap();
    let tree = Tree::from_entries(vec![
        TreeEntry::file("src.rs", blob, false).expect("valid fixture path"),
    ]);
    let tree = repo.store().put_tree(&tree).unwrap();
    let state = State::new(
        tree,
        vec![parent],
        Attribution::human(Principal::new("Fixture", "fixture@example.com")),
    )
    .with_intent(intent);
    repo.store().put_state(&state).unwrap();
    state.id()
}

fn directory_snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, output: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                visit(root, &entry, output);
            } else {
                output.push((
                    entry.strip_prefix(root).unwrap().display().to_string(),
                    fs::read(entry).unwrap(),
                ));
            }
        }
    }

    let mut output = Vec::new();
    visit(root, root, &mut output);
    output
}
