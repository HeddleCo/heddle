use super::*;

const BASE: &str = "fn capture(repo: &Repository) { let plan = SavePlan { git_scope: GitScope::None }; }";
const SOURCE: &str = "fn capture(repo: &Repository) { let git_overlay = repo.capability() == RepositoryCapability::GitOverlay; let plan = SavePlan { git_scope: if git_overlay { GitScope::WorktreeAll } else { GitScope::None } }; }";

#[test]
fn capture_has_one_origin_and_two_branch_comparisons() {
    let result = compare_file("save.rs", Some(BASE), Some(SOURCE), &[]);
    assert_eq!(result.changes.len(), 1, "capture must expose its conditional value change");
    let change = &result.changes[0];
    assert_eq!(change.symbol_address, "capture");
    assert_eq!(change.support, Support::Supported);
    let old: Vec<_> = change.expressions.iter().filter(|e| e.source.side == Side::Base).collect();
    assert_eq!(old.len(), 1, "the original value has one identity and no invented branches");
    assert!(old[0].conditional.is_none());
    let conditional = change.expressions.iter().find_map(|e| e.conditional.as_ref()).expect("current conditional");
    assert_eq!(conditional.branches.iter().map(|b| b.label).collect::<Vec<_>>(), vec![BranchLabel::True, BranchLabel::False]);
    assert_eq!(change.correspondences.len(), 2);
    assert_eq!(change.correspondences[0].kind, CorrespondenceKind::Replaced);
    assert_eq!(change.correspondences[1].kind, CorrespondenceKind::Retained);
    assert!(change.correspondences.iter().all(|c| c.base_ids == [old[0].id.clone()]));
    let binding = &change.bindings[0];
    assert_eq!(binding.name, "git_overlay");
    let resolved = change.expressions.iter().find(|e| Some(&e.id) == binding.value_id.as_ref()).expect("binding expression");
    assert_eq!(resolved.source.text, "repo.capability() == RepositoryCapability::GitOverlay");
}
