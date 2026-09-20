//! Capability-walk regression pins for the spool authz collapse (weft#1130).
//!
//! The Biscuit side answers "does this token's right at some ancestor cover
//! this resource?" by walking [`crate::resource::resolve_parent`]. That walk
//! exists in TWO places today: [`BiscuitFacts::has_action_through_inheritance`]
//! (the `can_read`/`can_write`/`is_admin_on` gate) and the
//! free-standing `authority_can_read` used to bound a limited ObserveIdentity scope.
//! weft#1130 collapses them onto one helper.
//!
//! `spool` is the sole resource fact vocabulary for both container and project
//! facets. `thread` and `context` remain distinct and rejoin the spool chain at
//! their parent.
//!
//! Every assertion here is a literal decision, not a re-derivation. A changed
//! literal is a permission change.

use super::*;
use crate::resource::{ResourceKind, resolve_parent};

/// A `right(kind, path, action)` triple in the shape the fixtures use.
type RightSpec = (&'static str, &'static str, &'static str);

fn rights_of(specs: &[RightSpec]) -> Vec<Right> {
    specs
        .iter()
        .map(|(kind, path, action)| Right::new(*kind, *path, *action))
        .collect()
}

fn facts(specs: &[RightSpec], is_staff: bool) -> BiscuitFacts {
    facts_with(rights_of(specs), is_staff)
}

// ── the chain step ─────────────────────────────────────────────────────────

/// Every spool takes the same parent step: strip the trailing path segment.
#[test]
fn every_spool_takes_the_same_parent_step() {
    for (path, expected) in [
        ("org/acme/heddle", Some("org/acme")),
        ("org/acme/sub/heddle", Some("org/acme/sub")),
        ("alice/notes", Some("alice")),
        ("a/b/c/d/e", Some("a/b/c/d")),
        ("", None),
        ("/", None),
        ("solo", None),
    ] {
        assert_eq!(
            resolve_parent(ResourceKind::Spool, path),
            expected.map(|parent| (ResourceKind::Spool, parent)),
            "{path:?}: unexpected spool parent",
        );
    }
}

/// The chain terminates, and it terminates at a ROOT segment — a single
/// segment has no parent, so a grant at `org` is the top of the acme tree and
/// nothing above it exists to inherit from.
#[test]
fn the_chain_terminates_at_a_single_segment() {
    assert_eq!(
        resolve_parent(ResourceKind::Spool, "org/acme/heddle"),
        Some((ResourceKind::Spool, "org/acme"))
    );
    assert_eq!(
        resolve_parent(ResourceKind::Spool, "org/acme"),
        Some((ResourceKind::Spool, "org"))
    );
    assert_eq!(resolve_parent(ResourceKind::Spool, "org"), None);
}

// ── the decision table ─────────────────────────────────────────────────────

/// Inheritance through the Biscuit rights, pinned as literals across the same
/// shapes the registry-side table covers: an org-kind grant, a project-kind
/// grant, nesting, cross-owner isolation, and the deny cases.
#[test]
fn inherited_capabilities_are_pinned_for_every_shape() {
    // An admin grant at a container covers everything under it, at any depth,
    // for read/write/admin — and covers the container itself.
    let org_admin = facts(&[("spool", "org/acme", "admin")], false);
    for (kind, path) in [
        ("spool", "org/acme"),
        ("spool", "org/acme/eng"),
        ("spool", "org/acme/heddle"),
        ("spool", "org/acme/eng/svc"),
        ("thread", "org/acme/heddle/threads/main"),
        ("context", "org/acme/heddle"),
    ] {
        assert!(org_admin.can_read(kind, path), "read {kind}:{path}");
        assert!(org_admin.can_write(kind, path), "write {kind}:{path}");
        assert!(org_admin.is_admin_on(kind, path), "admin {kind}:{path}");
    }
    // ...and nothing outside it: not a sibling org, not the root above it.
    for (kind, path) in [
        ("spool", "org"),
        ("spool", "org/other"),
        ("spool", "org/other/heddle"),
        ("spool", "alice/notes"),
    ] {
        assert!(
            !org_admin.can_read(kind, path),
            "must deny read {kind}:{path}"
        );
        assert!(
            !org_admin.is_admin_on(kind, path),
            "must deny admin {kind}:{path}"
        );
    }

    // A project-scoped write covers that project and its thread/context
    // children, and confers nothing on the container above it or on a sibling.
    let spool_write = facts(&[("spool", "org/acme/heddle", "write")], false);
    assert!(spool_write.can_write("spool", "org/acme/heddle"));
    assert!(spool_write.can_read("spool", "org/acme/heddle"));
    assert!(spool_write.can_write("thread", "org/acme/heddle/threads/main"));
    assert!(spool_write.can_write("context", "org/acme/heddle"));
    assert!(!spool_write.is_admin_on("spool", "org/acme/heddle"));
    assert!(!spool_write.can_read("spool", "org/acme"));
    assert!(!spool_write.can_read("spool", "org/acme/other"));

    // Cross-owner: a role in one tree never leaks into another.
    let cross = facts(
        &[
            ("spool", "org/acme", "admin"),
            ("spool", "alice/notes", "read"),
        ],
        false,
    );
    assert!(cross.can_read("spool", "alice/notes"));
    assert!(!cross.can_write("spool", "alice/notes"));
    assert!(!cross.can_read("spool", "alice"));
    assert!(cross.is_admin_on("spool", "org/acme/heddle"));

    // An unknown kind gets literal matching only — no chain walk is defined
    // for it, so an ancestor grant must not cover it.
    let unknown = facts(&[("spool", "org/acme", "admin")], false);
    assert!(!unknown.can_read("widget", "org/acme/thing"));

    // Staff short-circuits ordinary access capabilities.
    let staff = facts(&[], true);
    assert!(staff.can_read("spool", "anything/at/all"));
    assert!(staff.is_admin_on("spool", "org/acme"));
}

/// The two walks must ALREADY agree — this is the premise the collapse rests
/// on. `authority_can_read` bounds a limited ObserveIdentity scope over a raw
/// `&[Right]`; `can_read` gates real reads over the same rights. They quantify
/// differently (one walks once checking three actions per node, the other
/// walks three times) and must still decide identically.
#[test]
fn the_two_inheritance_walks_agree_on_every_probe() {
    let fixtures: &[(&str, &[RightSpec], bool)] = &[
        ("empty", &[], false),
        ("org admin", &[("spool", "org/acme", "admin")], false),
        ("repo read", &[("spool", "org/acme/heddle", "read")], false),
        (
            "repo write",
            &[("spool", "org/acme/heddle", "write")],
            false,
        ),
        (
            "mixed cross-owner",
            &[
                ("spool", "org/acme", "admin"),
                ("spool", "alice/notes", "read"),
                ("thread", "org/other/x/threads/t", "write"),
            ],
            false,
        ),
        ("staff", &[], true),
        ("wildcard spool admin", &[("spool", "*", "admin")], false),
    ];

    let probes: &[(&str, &str)] = &[
        ("spool", "org"),
        ("spool", "org/acme"),
        ("spool", "org/acme/eng"),
        ("spool", "alice"),
        ("spool", "org/acme/heddle"),
        ("spool", "org/acme/eng/svc"),
        ("spool", "org/other/heddle"),
        ("spool", "alice/notes"),
        ("thread", "org/acme/heddle/threads/main"),
        ("thread", "org/other/x/threads/t"),
        ("context", "org/acme/heddle"),
        ("widget", "org/acme/heddle"),
        ("spool", "*"),
    ];

    for (label, rights, is_staff) in fixtures {
        let built = rights_of(rights);
        let gate = facts(rights, *is_staff);
        for (kind, path) in probes {
            assert_eq!(
                authority_can_read(&built, *is_staff, kind, path),
                gate.can_read(kind, path),
                "{label}: the ObserveIdentity-bound walk and the read gate disagree on {kind}:{path}",
            );
        }
    }
}

/// The negative case, proving these pins CAN fail: a right one step to the
/// SIDE of the chain must never confer access. If the walk were widened from
/// "ancestors" to "any right whose path is a prefix-or-suffix match", or to
/// "holds any right at all", this is what catches it.
#[test]
fn a_sibling_right_never_confers_access() {
    let sibling = facts(&[("spool", "org/acme/heddle", "admin")], false);

    // The sibling project, the container above, and a deeper path that merely
    // shares a prefix with the granted one.
    assert!(!sibling.can_read("spool", "org/acme/other"));
    assert!(!sibling.can_read("spool", "org/acme"));
    assert!(!sibling.can_read("spool", "org/acme/heddle-staging"));

    // And a descendant-shaped path under a DIFFERENT root that happens to end
    // in the same segments.
    assert!(!sibling.can_read("spool", "other/acme/heddle"));
}

/// Declared device/service scope is an intersection with live grants, not a
/// replacement for them. Empty rights stay on the live-grant wildcard
/// (`spool:*`); a concrete `spool:{path}` binding must not leak to a sibling.
#[test]
fn declared_spool_scope_intersects_live_grants() {
    let wildcard = facts(&[], false);
    assert!(
        wildcard.covers_declared_spool("org/other/repo"),
        "a full session with no concrete spool rights keeps live-grant authority"
    );

    let scoped = facts(&[("spool", "org/acme/heddle", "read")], false);
    assert!(scoped.covers_declared_spool("org/acme/heddle"));
    assert!(scoped.covers_declared_spool("org/acme/heddle/child"));
    assert!(
        !scoped.covers_declared_spool("org/acme/other"),
        "a spool-scoped credential must not inherit the subject's other live grants"
    );

    let staff = facts(&[], true);
    assert!(staff.covers_declared_spool("anything/at/all"));
}
