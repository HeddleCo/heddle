// SPDX-License-Identifier: Apache-2.0

use std::fs;

use heddle_semantic_recovery::{
    BGE_SMALL_ARTIFACT_SHA256, BgeSmallEmbedder, ModelIdentity, RecoveryError, RecoveryIndex,
    ResidualQuantizerConfig, StateDocument, StateKey,
};

fn key(value: u8) -> StateKey {
    StateKey([value; 32])
}

fn model() -> ModelIdentity {
    ModelIdentity {
        id: "fixture-model".to_string(),
        artifact_sha256: "fixture-digest".to_string(),
        dimensions: 4,
    }
}

fn corpus() -> (Vec<StateDocument>, Vec<Vec<f32>>) {
    (
        vec![
            StateDocument {
                state: key(1),
                thread: "label-must-not-leak".to_string(),
                text: "alpha query".to_string(),
            },
            StateDocument {
                state: key(2),
                thread: "alpha".to_string(),
                text: "alpha rename".to_string(),
            },
            StateDocument {
                state: key(3),
                thread: "alpha".to_string(),
                text: "alpha reorder".to_string(),
            },
            StateDocument {
                state: key(4),
                thread: "beta".to_string(),
                text: "beta query".to_string(),
            },
            StateDocument {
                state: key(5),
                thread: "beta".to_string(),
                text: "beta insert".to_string(),
            },
            StateDocument {
                state: key(6),
                thread: "beta".to_string(),
                text: "beta comments".to_string(),
            },
        ],
        vec![
            vec![1.0, 0.01, 0.0, 0.0],
            vec![0.99, 0.02, 0.0, 0.0],
            vec![0.98, 0.03, 0.0, 0.0],
            vec![0.0, 1.0, 0.01, 0.0],
            vec![0.0, 0.99, 0.02, 0.0],
            vec![0.0, 0.98, 0.03, 0.0],
        ],
    )
}

#[test]
fn reconstruction_uses_neighbors_not_the_query_label() {
    let (documents, vectors) = corpus();
    let (index, report) = RecoveryIndex::build_from_embeddings(
        &documents,
        model(),
        vectors.clone(),
        ResidualQuantizerConfig::default(),
    )
    .unwrap();

    let result = index
        .reconstruct_thread(key(1), &vectors[0], 4)
        .unwrap()
        .unwrap();
    assert_eq!(result.thread, "alpha");
    assert_eq!(result.siblings.len(), 2);
    assert_eq!(report.theoretical_bits_per_vector, 9.0);
    assert_eq!(report.packed_bits_per_vector, 9);
    assert!(
        index
            .reconstruct_thread(key(99), &vectors[0], 4)
            .unwrap()
            .is_none()
    );
}

#[test]
fn sidecar_roundtrip_is_deterministic_and_rebuildable() {
    let temporary = tempfile::tempdir().unwrap();
    let first_path = temporary.path().join("first.bin");
    let second_path = temporary.path().join("second.bin");
    let (documents, vectors) = corpus();

    for path in [&first_path, &second_path] {
        let (index, _) = RecoveryIndex::build_from_embeddings(
            &documents,
            model(),
            vectors.clone(),
            ResidualQuantizerConfig::default(),
        )
        .unwrap();
        index.save(path).unwrap();
    }
    assert_eq!(
        fs::read(&first_path).unwrap(),
        fs::read(&second_path).unwrap()
    );

    let loaded = RecoveryIndex::load(&first_path).unwrap();
    assert_eq!(loaded.model(), &model());
    assert_eq!(
        loaded
            .reconstruct_thread(key(4), &vectors[3], 2)
            .unwrap()
            .unwrap()
            .thread,
        "beta"
    );

    fs::remove_file(&first_path).unwrap();
    assert!(!first_path.exists());
    assert!(second_path.exists());
}

#[test]
fn corrupt_sidecar_fails_loud() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("index.bin");
    let (documents, vectors) = corpus();
    let (index, _) = RecoveryIndex::build_from_embeddings(
        &documents,
        model(),
        vectors,
        ResidualQuantizerConfig::default(),
    )
    .unwrap();
    index.save(&path).unwrap();
    let mut bytes = fs::read(&path).unwrap();
    bytes[24] ^= 0xff;
    fs::write(&path, bytes).unwrap();

    assert!(matches!(
        RecoveryIndex::load(path),
        Err(RecoveryError::InvalidSidecar(message)) if message.contains("checksum")
    ));
}

#[test]
fn local_model_loader_rejects_an_unpinned_artifact_before_inference() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(
        temporary.path().join("model_optimized.onnx"),
        b"not the pinned model",
    )
    .unwrap();

    assert!(matches!(
        BgeSmallEmbedder::from_model_dir(temporary.path()),
        Err(RecoveryError::ModelDigestMismatch { expected, .. })
            if expected == BGE_SMALL_ARTIFACT_SHA256
    ));
}
