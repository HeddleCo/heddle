// SPDX-License-Identifier: Apache-2.0
//! Proof that facet lineages are independent (Spool epic P2, weft#358).
//!
//! On ONE repo, operations recorded under two different facet scopes
//! (`content` vs `governance`) must maintain:
//!   1. independent oplog batch views (each facet sees only its own batches);
//!   2. independent undo (undoing a governance batch does not rewind content);
//!   3. independent HEADs/refs (each facet's HEAD lives under its own ref prefix
//!      and moving one never moves the other).
//!
//! Plus: the well-known Git/Heddle content-side behavior is unchanged — the
//! `content` facet composes to the same per-worktree scope token as before.

use objects::object::StateId;

fn state_id(value: u8) -> StateId {
    StateId::from_bytes([value; 32])
}
use oplog::{OpLogBackend, OpLogRecorder};
use repo::{Repository, SpoolFacet};
use tempfile::TempDir;

#[test]
fn facets_have_independent_scope_tokens() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).expect("init repo");

    let base = repo.op_scope();
    // Content is the default facet: byte-identical to the pre-facet scope token,
    // so all existing content/Git/Heddle oplog + undo behavior is preserved.
    assert_eq!(repo.op_scope_for_facet(&SpoolFacet::Content), base);

    let gov = repo.op_scope_for_facet(&SpoolFacet::Governance);
    let mem = repo.op_scope_for_facet(&SpoolFacet::Membership);
    assert_eq!(gov, format!("{base}/governance"));
    assert_eq!(mem, format!("{base}/membership"));
    assert_ne!(gov, base);
    assert_ne!(gov, mem);
}

#[test]
fn facets_have_independent_batch_numbering_and_undo() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).expect("init repo");
    let oplog = repo.oplog();

    let content_scope = repo.op_scope_for_facet(&SpoolFacet::Content);
    let gov_scope = repo.op_scope_for_facet(&SpoolFacet::Governance);

    // Two content ops, one governance op — interleaved, one shared repo.
    let c1 = state_id(1);
    let c2 = state_id(2);
    let g1 = state_id(3);

    oplog
        .record_snapshot(&c1, None, None, Some(&content_scope))
        .expect("content snapshot 1");
    oplog
        .record_snapshot(&g1, None, None, Some(&gov_scope))
        .expect("governance snapshot 1");
    oplog
        .record_snapshot(&c2, Some(&c1), None, Some(&content_scope))
        .expect("content snapshot 2");

    // Each facet sees only its own batches — independent per-scope numbering.
    let content_batches = oplog
        .recent_batches_scoped(10, Some(&content_scope))
        .expect("content batches");
    let gov_batches = oplog
        .recent_batches_scoped(10, Some(&gov_scope))
        .expect("governance batches");
    assert_eq!(content_batches.len(), 2, "content facet sees its 2 batches");
    assert_eq!(gov_batches.len(), 1, "governance facet sees its 1 batch");

    // Undoing the governance batch must NOT rewind content.
    let gov_undo = oplog
        .undo_batches_scoped(1, Some(&gov_scope))
        .expect("governance undo candidates");
    assert_eq!(gov_undo.len(), 1);
    oplog
        .mark_batch_undone(&gov_undo[0])
        .expect("undo governance batch");

    // Governance now has nothing to undo; content is untouched (still 2 to undo).
    assert_eq!(
        oplog
            .undo_batches_scoped(1, Some(&gov_scope))
            .expect("gov undo after")
            .len(),
        0,
        "governance undo consumed its only batch"
    );
    assert_eq!(
        oplog
            .undo_batches_scoped(10, Some(&content_scope))
            .expect("content undo after")
            .len(),
        2,
        "content undo state is unaffected by the governance undo"
    );

    // Redo is likewise facet-local: governance has one redo candidate, content none.
    assert_eq!(
        oplog
            .redo_batches_scoped(10, Some(&gov_scope))
            .expect("gov redo")
            .len(),
        1
    );
    assert_eq!(
        oplog
            .redo_batches_scoped(10, Some(&content_scope))
            .expect("content redo")
            .len(),
        0
    );
}

#[test]
fn facets_have_independent_heads_and_refs() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).expect("init repo");

    // The content facet HEAD is the physical `.heddle/HEAD` (unchanged path).
    let content_head = repo
        .facet_head(&SpoolFacet::Content, "main")
        .expect("content head")
        .expect("content facet always has a HEAD");

    // Named facets start with no HEAD (their thread ref does not yet exist).
    assert!(
        repo.facet_head(&SpoolFacet::Governance, "main")
            .expect("gov head read")
            .is_none(),
        "governance HEAD absent before any governance op"
    );
    assert!(
        repo.facet_head_state(&SpoolFacet::Membership, "main")
            .expect("mem head state")
            .is_none()
    );

    // Advance the governance HEAD — this moves only the governance thread ref.
    let gov_state = state_id(4);
    repo.set_facet_head(&SpoolFacet::Governance, "main", &gov_state)
        .expect("set governance head");

    assert_eq!(
        repo.facet_head_state(&SpoolFacet::Governance, "main")
            .expect("gov head after set"),
        Some(gov_state),
        "governance HEAD now resolves to the state we set"
    );

    // Membership HEAD is still absent — moving governance did not touch it.
    assert!(
        repo.facet_head_state(&SpoolFacet::Membership, "main")
            .expect("mem head after gov set")
            .is_none(),
        "membership facet HEAD unaffected by governance move"
    );

    // The content facet HEAD is unchanged — moving governance did not touch it.
    assert_eq!(
        repo.facet_head(&SpoolFacet::Content, "main")
            .expect("content head after gov set")
            .expect("content HEAD present"),
        content_head,
        "content facet HEAD unaffected by governance move"
    );

    // Governance's HEAD lives under its own ref prefix, distinct from content's.
    assert_eq!(
        SpoolFacet::Governance.thread_ref("main"),
        "refs/spool/governance/threads/main"
    );
    assert_ne!(
        SpoolFacet::Governance.thread_ref("main"),
        SpoolFacet::Content.thread_ref("main")
    );

    // set_facet_head is rejected for the content facet (its HEAD moves via
    // snapshot/goto, not this named-facet helper).
    assert!(
        repo.set_facet_head(&SpoolFacet::Content, "main", &gov_state)
            .is_err(),
        "content facet HEAD must not be moved via set_facet_head"
    );
}
