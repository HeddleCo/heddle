use super::*;

const BASE: &str =
    "fn capture(repo: &Repository) { let plan = SavePlan { git_scope: GitScope::None }; }";
const SOURCE: &str = "fn capture(repo: &Repository) { let git_overlay = repo.capability() == RepositoryCapability::GitOverlay; let plan = SavePlan { git_scope: if git_overlay { GitScope::WorktreeAll } else { GitScope::None } }; }";

#[test]
fn capture_has_one_origin_and_two_branch_comparisons() {
    let result = compare_file("save.rs", Some(BASE), Some(SOURCE), &[]);
    assert_eq!(
        result.changes.len(),
        1,
        "capture must expose its conditional value change"
    );
    let change = &result.changes[0];
    assert_eq!(change.symbol_address, "capture");
    assert_eq!(change.support, Support::Supported);
    let old: Vec<_> = change
        .expressions
        .iter()
        .filter(|e| e.source.side == Side::Base)
        .collect();
    assert_eq!(
        old.len(),
        1,
        "the original value has one identity and no invented branches"
    );
    assert!(old[0].conditional.is_none());
    let conditional = change
        .expressions
        .iter()
        .find_map(|e| e.conditional.as_ref())
        .expect("current conditional");
    assert_eq!(
        conditional
            .branches
            .iter()
            .map(|b| b.label)
            .collect::<Vec<_>>(),
        vec![BranchLabel::True, BranchLabel::False]
    );
    assert_eq!(change.correspondences.len(), 2);
    assert_eq!(change.correspondences[0].kind, CorrespondenceKind::Replaced);
    assert_eq!(change.correspondences[1].kind, CorrespondenceKind::Retained);
    assert!(
        change
            .correspondences
            .iter()
            .all(|c| c.base_ids == [old[0].id.clone()])
    );
    let binding = &change.bindings[0];
    assert_eq!(binding.name, "git_overlay");
    let resolved = change
        .expressions
        .iter()
        .find(|e| Some(&e.id) == binding.value_id.as_ref())
        .expect("binding expression");
    assert_eq!(
        resolved.source.text,
        "repo.capability() == RepositoryCapability::GitOverlay"
    );
}

#[test]
fn real_capture_revision_pair_is_navigable_and_self_contained() {
    let base = include_str!("../../tests/fixtures/behavior/capture-base.rs");
    let source = include_str!("../../tests/fixtures/behavior/capture-source.rs");
    let result = compare_file(
        "crates/verbs/src/save.rs",
        Some(base),
        Some(source),
        &["capture".into()],
    );
    let change = result
        .changes
        .iter()
        .find(|c| {
            c.assignments
                .iter()
                .any(|a| a.target == "SavePlan.git_scope")
        })
        .expect("real capture change");
    assert_eq!(
        change.support,
        Support::Supported,
        "{:?}",
        change.limitations
    );
    assert_eq!(
        change
            .correspondences
            .iter()
            .map(|c| c.kind)
            .collect::<Vec<_>>(),
        vec![CorrespondenceKind::Replaced, CorrespondenceKind::Retained]
    );
    check_locations(change, base, source);
    assert!(
        !result.omitted.is_empty(),
        "this one pattern is not full file coverage"
    );
}

fn check_locations(change: &Change, base: &str, source: &str) {
    let locations = change
        .operations
        .iter()
        .map(|o| &o.source)
        .chain(change.assignments.iter().map(|a| &a.source))
        .chain(change.expressions.iter().map(|e| &e.source))
        .chain(
            change
                .expressions
                .iter()
                .filter_map(|e| e.conditional.as_ref())
                .flat_map(|c| c.branches.iter().map(|b| &b.source)),
        )
        .chain(
            change
                .bindings
                .iter()
                .filter_map(|b| b.declaration.as_ref()),
        );
    for location in locations {
        let text = if location.side == Side::Base {
            base
        } else {
            source
        };
        assert_eq!(
            &text[location.span.start as usize..location.span.end as usize],
            location.text
        );
    }
    let ids: std::collections::BTreeSet<_> = change.expressions.iter().map(|e| &e.id).collect();
    for c in &change.correspondences {
        assert!(
            c.base_ids
                .iter()
                .chain(&c.source_ids)
                .all(|id| ids.contains(id))
        );
    }
}

#[test]
fn unchanged_conditional_and_comment_edits_produce_no_change() {
    for source in [
        SOURCE.to_owned(),
        SOURCE
            .replace("GitScope::", "GitScope /* comment */ ::")
            .replace(";", ";\n"),
    ] {
        let result = compare_file("save.rs", Some(SOURCE), Some(&source), &[]);
        assert!(result.changes.is_empty());
        assert!(result.exhausted);
        assert!(!result.analyzed.is_empty());
    }
    let result = compare_file(
        "save.rs",
        Some(BASE),
        Some(&SOURCE.replace("GitScope::None", "GitScope /* retained */ :: None")),
        &[],
    );
    assert_eq!(
        result.changes[0].correspondences[1].kind,
        CorrespondenceKind::Retained
    );
}

#[test]
fn lexical_shadowing_resolves_the_nearest_prior_declaration() {
    let source = SOURCE.replace(
        "let git_overlay =",
        "let git_overlay = false; let git_overlay =",
    );
    let result = compare_file("save.rs", Some(BASE), Some(&source), &[]);
    let c = &result.changes[0];
    let binding = c
        .bindings
        .iter()
        .find(|b| b.name == "git_overlay")
        .expect("local binding");
    let value = c
        .expressions
        .iter()
        .find(|e| Some(&e.id) == binding.value_id.as_ref())
        .expect("resolved expression");
    assert!(value.source.text.contains("repo.capability()"));
    check_locations(c, BASE, &source);
}

#[test]
fn inner_scope_does_not_leak_and_later_shadow_does_not_rebind_earlier_predicate() {
    let source = SOURCE
        .replace("let plan", "{ let git_overlay = false; } let plan")
        .replace("; }", "; let git_overlay = false; }");
    let result = compare_file("save.rs", Some(BASE), Some(&source), &[]);
    let c = &result.changes[0];
    let b = c
        .bindings
        .iter()
        .find(|b| b.name == "git_overlay")
        .expect("binding");
    assert!(
        c.expressions
            .iter()
            .find(|e| Some(&e.id) == b.value_id.as_ref())
            .expect("value")
            .source
            .text
            .contains("repo.capability()")
    );
}

#[test]
fn changed_binding_is_a_change_even_if_conditional_tokens_are_identical() {
    let source = SOURCE.replace(
        "RepositoryCapability::GitOverlay",
        "RepositoryCapability::Native",
    );
    let result = compare_file("save.rs", Some(SOURCE), Some(&source), &[]);
    assert_eq!(result.changes.len(), 1);
    assert_eq!(
        result.changes[0].correspondences[0].kind,
        CorrespondenceKind::Replaced
    );
    assert!(
        result.changes[0].correspondences[1..]
            .iter()
            .all(|c| c.kind == CorrespondenceKind::Retained)
    );
}

#[test]
fn mutable_and_reassigned_predicates_are_explicitly_unsupported() {
    for source in [
        SOURCE.replace("let git_overlay", "let mut git_overlay"),
        SOURCE.replace("let plan", "git_overlay = false; let plan"),
    ] {
        let result = compare_file("save.rs", Some(BASE), Some(&source), &[]);
        let c = &result.changes[0];
        assert_eq!(c.support, Support::Partial);
        assert!(c.limitations.contains(&Limitation::MutableBinding));
        assert!(
            c.correspondences
                .iter()
                .all(|c| c.kind == CorrespondenceKind::Unmatched)
        );
    }
}

#[test]
fn duplicate_targets_remain_ambiguous() {
    let base = BASE.replace(
        "let plan",
        "let other = SavePlan { git_scope: GitScope::None }; let plan",
    );
    let result = compare_file("save.rs", Some(&base), Some(SOURCE), &[]);
    let c = &result.changes[0];
    assert!(c.limitations.contains(&Limitation::AmbiguousMatch));
    assert_eq!(c.correspondences[0].kind, CorrespondenceKind::Ambiguous);
    assert_eq!(c.correspondences[0].base_ids.len(), 2);
}

#[test]
fn unsupported_or_missing_inputs_never_look_like_empty_success() {
    for (path, source, reason) in [
        ("save.rs", Some("fn broken( {"), Limitation::ParseError),
        ("save.py", Some(SOURCE), Limitation::Language),
        ("save.rs", None, Limitation::MissingSource),
    ] {
        let result = compare_file(path, Some(BASE), source, &[]);
        assert!(result.changes.is_empty());
        assert_eq!(result.omitted[0].limitations, [reason]);
    }
    let oversized = " ".repeat(MAX_SOURCE_BYTES + 1);
    let result = compare_file("save.rs", Some(BASE), Some(&oversized), &[]);
    assert!(!result.exhausted);
    assert_eq!(result.omitted[0].limitations, [Limitation::Budget]);
}

#[test]
fn missing_else_and_unsupported_branch_statements_keep_syntax_without_claimed_replacement() {
    for (source, reason) in [
        (
            SOURCE.replace(" else { GitScope::None }", ""),
            Limitation::MissingElse,
        ),
        (
            SOURCE.replace(
                "{ GitScope::WorktreeAll }",
                "{ do_work(); GitScope::WorktreeAll }",
            ),
            Limitation::Syntax,
        ),
    ] {
        let result = compare_file("save.rs", Some(BASE), Some(&source), &[]);
        assert!(result.changes[0].limitations.contains(&reason));
        assert_eq!(
            result.changes[0].correspondences[0].kind,
            CorrespondenceKind::Unmatched
        );
    }
}

#[test]
fn assignments_and_existing_conditionals_use_the_same_model() {
    let base = "fn capture() { let enabled = true; plan.git_scope = if enabled { A } else { B }; }";
    let source = base.replace("{ A }", "{ C }");
    let result = compare_file("save.rs", Some(base), Some(&source), &[]);
    let c = &result.changes[0];
    assert_eq!(c.assignments[0].kind, AssignmentKind::Assignment);
    assert_eq!(c.assignments[0].target, "plan.git_scope");
    assert_eq!(
        c.correspondences.iter().map(|c| c.kind).collect::<Vec<_>>(),
        vec![
            CorrespondenceKind::Retained,
            CorrespondenceKind::Replaced,
            CorrespondenceKind::Retained
        ]
    );
}

#[test]
fn ids_are_deterministic_but_source_navigation_invalidates_on_blob_change() {
    let result = compare_file("save.rs", Some(BASE), Some(SOURCE), &[]);
    assert_eq!(
        result,
        compare_file("save.rs", Some(BASE), Some(SOURCE), &[])
    );
    let moved = compare_file(
        "save.rs",
        Some(BASE),
        Some(&format!("// heading\n{SOURCE}")),
        &[],
    );
    assert_ne!(result.source_blob, moved.source_blob);
    assert_ne!(result.changes[0].id, moved.changes[0].id);
    assert_ne!(
        result.changes[0].expressions.last().expect("expression").id,
        moved.changes[0].expressions.last().expect("expression").id
    );
    assert_eq!(
        result.changes[0]
            .operations
            .last()
            .expect("operation")
            .symbol
            .semantic_hash,
        moved.changes[0]
            .operations
            .last()
            .expect("operation")
            .symbol
            .semantic_hash
    );
}

#[test]
fn compound_predicate_depends_on_lexically_resolved_bindings() {
    let base = SOURCE.replace("if git_overlay", "if git_overlay && true");
    let source = base.replace(
        "RepositoryCapability::GitOverlay",
        "RepositoryCapability::Native",
    );
    let result = compare_file("save.rs", Some(&base), Some(&source), &[]);
    assert_eq!(result.changes.len(), 1);
    assert_eq!(
        result.changes[0].correspondences[0].kind,
        CorrespondenceKind::Replaced
    );
}

#[test]
fn truncated_target_inventory_cannot_produce_confident_correspondence() {
    let source = format!(
        "fn f() {{ {} }}",
        "let x = Plan { value: if true { A } else { B } };".repeat(MAX_ASSIGNMENTS + 1)
    );
    let result = compare_file("save.rs", Some(BASE), Some(&source), &[]);
    assert!(!result.exhausted);
    assert!(result.changes.is_empty());
    assert_eq!(result.omitted[0].limitations, [Limitation::Budget]);
}

#[test]
fn predicate_binding_dependencies_include_local_expressions_transitively() {
    let base = "fn f() { let kind = A; let enabled = kind == A; let p = P { v: if enabled { X } else { Y } }; }";
    let source = base.replace("let kind = A", "let kind = B");
    let result = compare_file("source.rs", Some(base), Some(&source), &[]);
    assert_eq!(
        result.changes.len(),
        1,
        "a dependency of the resolved predicate changed"
    );
    assert_eq!(
        result.changes[0].correspondences[0].kind,
        CorrespondenceKind::Replaced
    );
    assert!(result.changes[0].bindings.iter().any(|b| b.name == "kind"));
}

#[test]
fn unsupported_while_pattern_does_not_resolve_an_outer_homonym() {
    let base = "fn f() { let x = false; while let Some(x) = next() { p.v = A; } }";
    let source = base.replace("p.v = A", "p.v = if x { B } else { A }");
    let result = compare_file("source.rs", Some(base), Some(&source), &[]);
    assert!(result.changes[0].limitations.contains(&Limitation::Syntax));
    assert_eq!(
        result.changes[0].correspondences[0].kind,
        CorrespondenceKind::Unmatched
    );
}

#[test]
fn generic_target_formatting_does_not_invent_added_or_removed_assignments() {
    let base = SOURCE.replace("SavePlan", "SavePlan::<u8>");
    let source = base.replace("SavePlan::<u8>", "SavePlan :: < /* type */ u8 >");
    let result = compare_file("save.rs", Some(&base), Some(&source), &[]);
    assert!(result.changes.is_empty());
    assert!(result.omitted.is_empty());
    assert!(!result.analyzed.is_empty());
}
