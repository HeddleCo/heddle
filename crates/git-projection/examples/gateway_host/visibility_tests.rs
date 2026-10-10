// SPDX-License-Identifier: Apache-2.0
//! Independent boundary tests. A valid test receipt is not a hosted authority implementation.

use crate::policy::{Config, Disclosure, Manifest, canonical, digest, now};
use crate::projection::Cache;
use std::collections::BTreeMap;

fn fixture() -> (Config, Manifest, Disclosure) {
    let manifest = Manifest {
        schema: 1,
        repository: "synthetic".into(),
        source: "native-demo".into(),
        thread: "main".into(),
        state: format!("hs-{}", "a".repeat(52)),
        git_oid: "a".repeat(40),
        mode: "snapshot".into(),
        policy_epoch: 1,
    };
    let instant = now().expect("test clock");
    let threads = vec!["main".to_string(), "source".to_string()];
    let config = Config {
        schema: 1,
        expires_at: instant + 600,
        published_pins: vec!["b".repeat(40)],
        reader_sha256: "c".repeat(64),
        service_sha256: "d".repeat(64),
        views: vec![manifest.clone()],
        descriptors: BTreeMap::from([("native-demo".into(), "e".repeat(64))]),
        authorized_threads: BTreeMap::from([("native-demo".into(), threads.clone())]),
    };
    let disclosure = Disclosure {
        schema: 1,
        authority: "heddle-current-disclosure".into(),
        allow: true,
        audience: "public".into(),
        reader_sha256: config.reader_sha256.clone(),
        pin: config.published_pins[0].clone(),
        manifest_sha256: digest(&canonical(&manifest).expect("manifest bytes")),
        native_sha256: config.descriptors["native-demo"].clone(),
        threads_sha256: digest(&canonical(&threads).expect("Thread grant bytes")),
        generation: "f".repeat(64),
        expires_at: instant + 20,
    };
    (config, manifest, disclosure)
}

#[test]
fn hosted_disclosure_cannot_accept_synthetic_or_wrong_scope() {
    let (config, manifest, disclosure) = fixture();
    let pin = config.published_pins[0].as_str();
    disclosure
        .validate(&config, &manifest, pin, false)
        .expect("matching receipt");
    let mutations: Vec<fn(&mut Disclosure)> = vec![
        |d| d.schema = 2,
        |d| d.allow = false,
        |d| d.authority = "quiescent-synthetic".into(),
        |d| d.authority.clear(),
        |d| d.audience = "internal".into(),
        |d| d.audience = "restricted:security".into(),
        |d| d.reader_sha256 = "0".repeat(64),
        |d| d.pin = "0".repeat(40),
        |d| d.manifest_sha256 = "0".repeat(64),
        |d| d.native_sha256 = "0".repeat(64),
        |d| d.threads_sha256 = "0".repeat(64),
        |d| d.generation = "A".repeat(64),
        |d| d.generation.clear(),
        |d| d.expires_at = now().expect("clock"),
        |d| d.expires_at = now().expect("clock") + 120,
    ];
    for (index, mutate) in mutations.into_iter().enumerate() {
        let mut changed = disclosure.clone();
        mutate(&mut changed);
        assert!(
            changed.validate(&config, &manifest, pin, false).is_err(),
            "mutation {index} admitted"
        );
    }
}

#[test]
fn fixture_mode_requires_its_explicit_authority_marker() {
    let (config, manifest, mut disclosure) = fixture();
    let pin = config.published_pins[0].as_str();
    assert!(disclosure.validate(&config, &manifest, pin, true).is_err());
    disclosure.authority = "quiescent-synthetic".into();
    disclosure
        .validate(&config, &manifest, pin, true)
        .expect("explicit local synthetic receipt");
    assert!(disclosure.validate(&config, &manifest, pin, false).is_err());
}

#[test]
fn authority_cannot_extend_config_lifetime() {
    let (mut config, manifest, mut disclosure) = fixture();
    config.expires_at = now().expect("clock") + 5;
    disclosure.expires_at = config.expires_at + 1;
    assert!(
        disclosure
            .validate(&config, &manifest, &config.published_pins[0], false)
            .is_err()
    );
}

#[test]
fn final_decision_must_preserve_scope_and_generation() {
    let (_config, _manifest, disclosure) = fixture();
    let mut renewal = disclosure.clone();
    renewal.expires_at += 1;
    assert!(
        disclosure.same_generation(&renewal),
        "expiry alone can renew"
    );
    let mutations: Vec<fn(&mut Disclosure)> = vec![
        |d| d.schema = 2,
        |d| d.allow = false,
        |d| d.authority = "quiescent-synthetic".into(),
        |d| d.audience = "internal".into(),
        |d| d.reader_sha256 = "0".repeat(64),
        |d| d.pin = "0".repeat(40),
        |d| d.manifest_sha256 = "0".repeat(64),
        |d| d.native_sha256 = "0".repeat(64),
        |d| d.threads_sha256 = "0".repeat(64),
        |d| d.generation = "0".repeat(64),
    ];
    for (index, mutate) in mutations.into_iter().enumerate() {
        let mut changed = renewal.clone();
        mutate(&mut changed);
        assert!(
            !disclosure.same_generation(&changed),
            "mutation {index} survived final comparison"
        );
    }
}

#[test]
fn cache_key_separates_every_authorized_projection_boundary() {
    let (config, manifest, disclosure) = fixture();
    let original = Cache::key(&config, &manifest, &disclosure).expect("cache identity");
    let manifest_mutations: Vec<fn(&mut Manifest)> = vec![
        |m| m.repository = "other".into(),
        |m| m.thread = "source".into(),
        |m| m.state = format!("hs-{}", "b".repeat(52)),
        |m| m.git_oid = "0".repeat(40),
        |m| m.mode = "history".into(),
        |m| m.policy_epoch = 2,
    ];
    for (index, mutate) in manifest_mutations.into_iter().enumerate() {
        let mut changed = manifest.clone();
        mutate(&mut changed);
        assert_ne!(
            original,
            Cache::key(&config, &changed, &disclosure).expect("key"),
            "manifest mutation {index}"
        );
    }
    let decision_mutations: Vec<fn(&mut Disclosure)> = vec![
        |d| d.authority = "quiescent-synthetic".into(),
        |d| d.audience = "internal".into(),
        |d| d.reader_sha256 = "0".repeat(64),
        |d| d.generation = "0".repeat(64),
    ];
    for (index, mutate) in decision_mutations.into_iter().enumerate() {
        let mut changed = disclosure.clone();
        mutate(&mut changed);
        assert_ne!(
            original,
            Cache::key(&config, &manifest, &changed).expect("key"),
            "decision mutation {index}"
        );
    }
    let mut changed_source = config.clone();
    changed_source
        .descriptors
        .insert(manifest.source.clone(), "0".repeat(64));
    assert_ne!(
        original,
        Cache::key(&changed_source, &manifest, &disclosure).expect("changed source")
    );
    let mut changed_threads = config.clone();
    changed_threads
        .authorized_threads
        .insert(manifest.source.clone(), vec!["main".into()]);
    assert_ne!(
        original,
        Cache::key(&changed_threads, &manifest, &disclosure).expect("reduced closure")
    );
}

#[test]
fn cache_identity_is_stable_across_grant_order_and_receipt_renewal() {
    let (mut config, manifest, mut disclosure) = fixture();
    let original = Cache::key(&config, &manifest, &disclosure).expect("key");
    config
        .authorized_threads
        .get_mut(&manifest.source)
        .expect("Threads")
        .reverse();
    disclosure.expires_at += 1;
    assert_eq!(
        original,
        Cache::key(&config, &manifest, &disclosure).expect("renewed key")
    );
}
