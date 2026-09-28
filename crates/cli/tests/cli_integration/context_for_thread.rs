// SPDX-License-Identifier: Apache-2.0
//! The pre-turn briefing and exact next-capture receipt.

use std::collections::BTreeSet;

use crypto::{Signer, thread_operation::SignedOperation};
use objects::object::{
    CollaborationActor, ContentHash, ThreadName,
    thread_replication::{
        ThreadOperation, ThreadOperationBody,
        metadata::{AUTHORITY_FORMAT, Control, Intent, ThreadControl},
    },
};
use serde_json::Value;
use tempfile::TempDir;

use super::{Repository, heddle, seed_test_repo_principal};

fn fixture() -> TempDir {
    let temp = TempDir::new().expect("temporary repository");
    heddle(&["init"], Some(temp.path())).expect("init");
    seed_test_repo_principal(temp.path()).expect("principal");
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").expect("source");
    heddle(&["capture", "-m", "seed"], Some(temp.path())).expect("seed");
    heddle(&["thread", "create", "brief"], Some(temp.path())).expect("thread");
    heddle(&["thread", "switch", "brief"], Some(temp.path())).expect("switch");
    heddle(
        &[
            "context",
            "set",
            "--path",
            "main.rs",
            "--kind",
            "constraint",
            "-m",
            "Keep the entry point",
        ],
        Some(temp.path()),
    )
    .expect("constraint");
    heddle(
        &[
            "discuss",
            "new",
            "--path",
            "main.rs",
            "--thread",
            "brief",
            "--title",
            "Check entry point",
            "-m",
            "Does this remain callable?",
        ],
        Some(temp.path()),
    )
    .expect("discussion");
    let repo = Repository::open(temp.path()).expect("open Thread repository");
    let replica = repo.native_thread("brief").expect("native Thread");
    let signer = repo.native_thread_signer(&replica).expect("Thread signer");
    let authority = b"briefing integration fixture";
    let control = ThreadControl {
        version: 1,
        spool: uuid::Uuid::parse_str(&replica.genesis().expect("genesis").spool).expect("spool"),
        actor: CollaborationActor {
            principal_id: uuid::Uuid::from_u128(2),
            agent_id: None,
        },
        authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, authority),
        authority_envelope: authority.to_vec(),
        client_operation_id: uuid::Uuid::from_u128(9),
        occurred_at_ms: 1000,
        control: Control::Intent(Intent {
            outcome: "Keep main callable".into(),
            acceptance_criteria: vec!["Entry point compiles".into()],
            origin_urls: Vec::new(),
            principal_approved: false,
        }),
    };
    let signed = SignedOperation::sign(
        &ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            publisher: signer.public_key().try_into().expect("publisher key"),
            parents: BTreeSet::new(),
            body: ThreadOperationBody::Metadata(control.encode().expect("intent")),
        },
        &signer,
    )
    .expect("signed intent");
    replica
        .receive_control_cas(&signed, repo.store(), |_| Ok(()))
        .expect("intent admission");
    temp
}

#[test]
fn context_for_thread_briefs_in_text_and_json() {
    let temp = fixture();
    let text =
        heddle(&["context", "--for-thread", "brief"], Some(temp.path())).expect("text briefing");
    assert!(text.contains("Thread: brief"), "{text}");
    assert!(text.contains("Keep main callable"), "{text}");
    assert!(text.contains("Entry point compiles"), "{text}");
    assert!(text.contains("Keep the entry point"), "{text}");
    assert!(text.contains("Check entry point"), "{text}");

    let json = heddle(
        &["--output", "json", "context", "--for-thread", "brief"],
        Some(temp.path()),
    )
    .expect("JSON briefing");
    let value: Value = serde_json::from_str(&json).expect("JSON");
    assert_eq!(value["output_kind"], "context_for_thread");
    assert_eq!(value["thread"], "brief");
    assert_eq!(value["intent"][0]["outcome"], "Keep main callable");
    assert_eq!(
        value["intent"][0]["acceptance_criteria"][0],
        "Entry point compiles"
    );
    assert_eq!(
        value["annotations"][0]["revisions"][0]["content"],
        "Keep the entry point"
    );
    assert_eq!(value["discussions"][0]["title"], "Check entry point");
    assert!(
        value["briefing_hash"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty())
    );
}

#[test]
fn next_agent_capture_records_exact_briefing_and_thread_show_displays_it() {
    let temp = fixture();
    let json = heddle(
        &["--output", "json", "context", "--for-thread", "brief"],
        Some(temp.path()),
    )
    .expect("briefing");
    let briefing: Value = serde_json::from_str(&json).expect("briefing JSON");
    let supplied_revision = briefing["annotations"][0]["revisions"][0]["revision_id"]
        .as_str()
        .expect("revision");
    std::fs::write(
        temp.path().join("main.rs"),
        "fn main() { println!(\"ready\"); }\n",
    )
    .expect("edit");
    heddle(
        &[
            "capture",
            "-m",
            "implement entry point",
            "--agent-provider",
            "codex",
            "--agent-model",
            "gpt-test",
        ],
        Some(temp.path()),
    )
    .expect("agent capture");

    let repo = Repository::open(temp.path()).expect("repo");
    let head = repo
        .refs()
        .get_thread(&ThreadName::new("brief"))
        .expect("thread ref")
        .expect("head");
    let receipt = repo
        .context_receipt(&head)
        .expect("read receipt")
        .expect("attached receipt");
    assert_eq!(
        receipt.briefing_hash,
        briefing["briefing_hash"].as_str().expect("brief hash")
    );
    assert_eq!(
        receipt.annotations[0].revisions[0].revision_id,
        supplied_revision
    );
    assert_eq!(
        receipt.annotations[0].revisions[0].content,
        "Keep the entry point"
    );
    assert_eq!(receipt.intent_versions.len(), 1);

    let shown = heddle(&["thread", "show", "brief"], Some(temp.path())).expect("show");
    assert!(shown.contains("Constraints supplied:"), "{shown}");
    assert!(shown.contains("1 annotation"), "{shown}");
    assert!(shown.contains("Keep the entry point"), "{shown}");
    assert!(
        !shown.contains(supplied_revision),
        "human output leaked a raw revision ID: {shown}"
    );
    let shown_json = heddle(
        &["--output", "json", "thread", "show", "brief"],
        Some(temp.path()),
    )
    .expect("show JSON");
    let shown: Value = serde_json::from_str(&shown_json).expect("show JSON payload");
    assert_eq!(
        shown["constraints_supplied"][0]["annotations"][0]["revisions"][0]["revision_id"],
        supplied_revision
    );
}
