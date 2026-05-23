// SPDX-License-Identifier: Apache-2.0
//! Thread stacks — first-class read API for the descendant tree of a
//! thread, formed by walking [`ThreadRecord::parent_thread`] links.
//!
//! A "stack" is the descendant tree of a single root thread. Roots are
//! threads whose `parent_thread` either is `None` or points at a name
//! not present in the supplied record list (e.g. `main` / a deleted
//! parent). That keeps stack discovery local and resilient to detached
//! refs — we never pretend a thread has a parent we can't see.
//!
//! The discovery side ([`compute_stacks`], [`stack_for`]) is read-only.
//! The planner ([`plan_stack_rebase`]) returns an ordered, BFS plan that
//! the existing single-thread rebase machinery executes one step at a
//! time; this module never mutates state.
//!
//! Conceptual ancestor: HeddleCo/weft#46. This is an adaptation, not a
//! cherry-pick — the original was written against the pre-#78 weft
//! crate layout. See `tests/stack_rebase.rs` (in `crates/cli`) for the
//! integration coverage.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::{Repository, Result, thread_model::ThreadRecord, thread_storage::ThreadManager};

/// One node in a stack tree. `name` matches [`ThreadRecord::thread`];
/// `children` are the immediate descendants in the same stack, sorted
/// by name for stable rendering.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StackNode {
    pub name: String,
    pub children: Vec<StackNode>,
}

impl StackNode {
    /// Total number of threads in this subtree, including the node itself.
    pub fn size(&self) -> usize {
        1 + self.children.iter().map(StackNode::size).sum::<usize>()
    }

    /// Depth of the deepest leaf below this node. A root with no children
    /// has depth 0.
    pub fn depth(&self) -> usize {
        self.children
            .iter()
            .map(|c| 1 + c.depth())
            .max()
            .unwrap_or(0)
    }

    /// Yield every thread name in the subtree, root first, depth-first.
    pub fn iter_names(&self) -> impl Iterator<Item = &str> {
        let mut stack: Vec<&StackNode> = vec![self];
        std::iter::from_fn(move || {
            let next = stack.pop()?;
            // Push children in reverse so the iterator yields them in the
            // original sorted order.
            for child in next.children.iter().rev() {
                stack.push(child);
            }
            Some(next.name.as_str())
        })
    }
}

/// One discovered stack — the root thread plus its full descendant tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadStack {
    pub root: StackNode,
}

impl ThreadStack {
    pub fn member_count(&self) -> usize {
        self.root.size()
    }

    pub fn depth(&self) -> usize {
        self.root.depth()
    }

    pub fn root_name(&self) -> &str {
        &self.root.name
    }

    /// Names of every thread in the stack, root first.
    pub fn member_names(&self) -> Vec<&str> {
        self.root.iter_names().collect()
    }

    /// Whether `thread_name` is part of this stack.
    pub fn contains(&self, thread_name: &str) -> bool {
        self.root.iter_names().any(|name| name == thread_name)
    }
}

/// Compute every stack in `records`. Returned stacks are sorted by root
/// name for deterministic output.
///
/// A stack is rooted at a record whose `parent_thread` is `None` or
/// names a thread not in the list. Cycles are skipped silently.
pub fn compute_stacks(records: &[ThreadRecord]) -> Vec<ThreadStack> {
    let by_name: BTreeMap<&str, &ThreadRecord> =
        records.iter().map(|r| (r.thread.as_str(), r)).collect();
    let children_of = children_index(records, &by_name);

    let roots: Vec<&str> = by_name
        .keys()
        .filter(|name| {
            let record = by_name[*name];
            match record.parent_thread.as_deref() {
                None => true,
                Some(parent) => !by_name.contains_key(parent),
            }
        })
        .copied()
        .collect();

    let mut stacks = Vec::with_capacity(roots.len());
    for root in roots {
        let mut visited = HashSet::new();
        if let Some(node) = build_node(root, &children_of, &mut visited) {
            stacks.push(ThreadStack { root: node });
        }
    }
    stacks
}

/// Find the stack containing `thread_name`. Returns `None` if the thread
/// doesn't exist in `records`.
///
/// Walks parents up to the stack root before computing the descendant
/// tree, so callers always get the full picture.
pub fn stack_for(records: &[ThreadRecord], thread_name: &str) -> Option<ThreadStack> {
    let by_name: BTreeMap<&str, &ThreadRecord> =
        records.iter().map(|r| (r.thread.as_str(), r)).collect();
    by_name.get(thread_name)?;

    let mut cursor: &str = thread_name;
    let mut seen: HashSet<&str> = HashSet::new();
    loop {
        if !seen.insert(cursor) {
            // Cycle — bail.
            return None;
        }
        let record = match by_name.get(cursor) {
            Some(r) => *r,
            None => break,
        };
        match record.parent_thread.as_deref() {
            Some(name) if by_name.contains_key(name) => {
                cursor = name;
            }
            _ => break,
        }
    }
    let root_name = cursor;

    let children_of = children_index(records, &by_name);
    let mut visited = HashSet::new();
    let node = build_node(root_name, &children_of, &mut visited)?;
    Some(ThreadStack { root: node })
}

fn children_index<'a>(
    records: &'a [ThreadRecord],
    by_name: &BTreeMap<&'a str, &'a ThreadRecord>,
) -> HashMap<&'a str, Vec<&'a str>> {
    let mut children_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for record in records {
        if let Some(parent) = record.parent_thread.as_deref()
            && by_name.contains_key(parent)
        {
            children_of
                .entry(parent)
                .or_default()
                .push(record.thread.as_str());
        }
    }
    for kids in children_of.values_mut() {
        kids.sort();
    }
    children_of
}

fn build_node<'a>(
    name: &'a str,
    children_of: &HashMap<&'a str, Vec<&'a str>>,
    visited: &mut HashSet<&'a str>,
) -> Option<StackNode> {
    if !visited.insert(name) {
        // Cycle protection — refuse to render the second occurrence.
        return None;
    }
    let children = children_of
        .get(name)
        .map(|kids| {
            kids.iter()
                .filter_map(|child| build_node(child, children_of, visited))
                .collect()
        })
        .unwrap_or_default();
    Some(StackNode {
        name: name.to_string(),
        children,
    })
}

// ── Rebase planning ──────────────────────────────────────────────────────

/// One thread's worth of work in a stack-rebase plan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StackRebaseStep {
    /// Thread ref name being moved.
    pub thread: String,
    /// State the thread points at today.
    pub current_state: String,
    /// State the thread is currently rebased on. Lets the executor compute
    /// the delta `(old_base..current_state]` to replay onto `new_base`.
    pub old_base: String,
    /// State the thread should be replayed onto. For the root this is
    /// the caller-supplied `onto`; for descendants it's the parent's
    /// projected new tip.
    pub new_base: String,
    /// Parent thread name, if any. Roots have `None`.
    pub parent_thread: Option<String>,
    /// Distance from the stack root. Root is 0.
    pub depth: usize,
}

/// Ordered, root-first plan for rebasing an entire stack.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StackRebasePlan {
    pub root_thread: String,
    pub onto: String,
    pub steps: Vec<StackRebaseStep>,
}

impl StackRebasePlan {
    pub fn step_count(&self) -> usize {
        self.steps.len()
    }

    /// `true` if every step's current state already matches its target —
    /// no thread needs to move. Lets callers print "already up to date".
    pub fn is_no_op(&self) -> bool {
        self.steps
            .iter()
            .all(|step| step.current_state == step.new_base)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanRebaseError {
    #[error("thread '{0}' not found")]
    ThreadNotFound(String),
    #[error("thread '{0}' is not a stack root (has parent '{1}'); rebase the root instead")]
    NotARoot(String, String),
}

/// Compute a rebase plan for the stack rooted at `root_thread`.
///
/// BFS-orders descendants so each child is planned after its parent has
/// already been assigned a new base. For descendants whose current state
/// equals their old base (no commits past the branch point), `new_base`
/// is inherited unchanged — those threads are no-ops and the executor
/// can fast-forward them.
///
/// `current_state_for(thread_name)` returns the thread's current tip,
/// passed in as a closure so the planner stays a pure function.
pub fn plan_stack_rebase<F>(
    records: &[ThreadRecord],
    root_thread: &str,
    onto: &str,
    mut current_state_for: F,
) -> std::result::Result<StackRebasePlan, PlanRebaseError>
where
    F: FnMut(&str) -> String,
{
    let by_name: BTreeMap<&str, &ThreadRecord> =
        records.iter().map(|r| (r.thread.as_str(), r)).collect();
    let root_record = by_name
        .get(root_thread)
        .ok_or_else(|| PlanRebaseError::ThreadNotFound(root_thread.to_string()))?;

    if let Some(parent) = root_record.parent_thread.as_deref()
        && by_name.contains_key(parent)
    {
        return Err(PlanRebaseError::NotARoot(
            root_thread.to_string(),
            parent.to_string(),
        ));
    }

    let children_of = children_index(records, &by_name);

    // BFS so children are visited only after their parents.
    let mut steps = Vec::new();
    let mut queue: VecDeque<(&str, usize, String)> = VecDeque::new();
    queue.push_back((root_thread, 0, onto.to_string()));

    while let Some((thread_name, depth, new_base)) = queue.pop_front() {
        let record = by_name[thread_name];
        let current = current_state_for(thread_name);
        // The thread's projected new tip = `new_base` if there are no
        // commits past the old base (no-op), otherwise a synthetic
        // "${thread}@projected" handle that descendants reference. The
        // executor replaces these with the real new change_id when each
        // rebase lands.
        let projected_tip = if current == record.base_state {
            new_base.clone()
        } else {
            format!("{thread_name}@projected")
        };

        // A step is a root iff `parent_thread.is_none()`. The source
        // record's `parent_thread` may point at a thread outside the
        // record set (e.g. `main`); for the planner that still counts
        // as a root and the field is informational/historical only, so
        // we erase it at depth 0 to make root-detection unambiguous
        // downstream.
        let parent_thread = if depth == 0 {
            None
        } else {
            record.parent_thread.clone()
        };

        steps.push(StackRebaseStep {
            thread: thread_name.to_string(),
            current_state: current,
            old_base: record.base_state.clone(),
            new_base: new_base.clone(),
            parent_thread,
            depth,
        });

        if let Some(kids) = children_of.get(thread_name) {
            for child in kids {
                queue.push_back((child, depth + 1, projected_tip.clone()));
            }
        }
    }

    Ok(StackRebasePlan {
        root_thread: root_thread.to_string(),
        onto: onto.to_string(),
        steps,
    })
}

// ── Repository extension methods ────────────────────────────────────────

impl Repository {
    /// Load every thread record from disk and compute every stack in the
    /// repo. Convenience wrapper around [`compute_stacks`] that handles
    /// the [`ThreadManager`] read.
    pub fn compute_thread_stacks(&self) -> Result<Vec<ThreadStack>> {
        let records = ThreadManager::new(self.heddle_dir()).list_records()?;
        Ok(compute_stacks(&records))
    }

    /// Find the stack containing `thread_name`, walking up to the root
    /// before computing the descendant tree. Returns `None` when the
    /// thread isn't in the corpus.
    pub fn thread_stack_for(&self, thread_name: &str) -> Result<Option<ThreadStack>> {
        let records = ThreadManager::new(self.heddle_dir()).list_records()?;
        Ok(stack_for(&records, thread_name))
    }

    /// Plan a rebase for the stack rooted at `root_thread`. The planner
    /// reads each thread's live tip via `refs.get_thread`, so it stays
    /// correct even when the on-disk `current_state` in
    /// [`ThreadRecord`] hasn't been refreshed yet.
    ///
    /// Returns `Ok(Err(PlanRebaseError))` for shape-level errors
    /// (unknown root, non-root target) so callers can distinguish a
    /// planner refusal from an I/O failure.
    pub fn plan_thread_stack_rebase(
        &self,
        root_thread: &str,
        onto: &str,
    ) -> Result<std::result::Result<StackRebasePlan, PlanRebaseError>> {
        let records = ThreadManager::new(self.heddle_dir()).list_records()?;
        // Pre-fetch live tips so ref I/O errors surface here instead of
        // being swallowed inside the (infallible) planner closure. A
        // truly-absent thread ref (Ok(None)) is allowed — the planner
        // falls back to the thread name as a sentinel — but an Err from
        // `get_thread` must propagate, otherwise downstream code would
        // act on a fake base/tip and could produce destructive plans.
        let refs = self.refs();
        let mut tips: HashMap<String, String> = HashMap::new();
        for record in &records {
            if let Some(id) = refs.get_thread(&record.thread)? {
                tips.insert(record.thread.clone(), id.to_string());
            }
        }
        let plan = plan_stack_rebase(&records, root_thread, onto, |name| {
            tips.get(name)
                .cloned()
                .unwrap_or_else(|| name.to_string())
        });
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::thread_model::{ThreadFreshness, ThreadMode, ThreadState};

    fn record(name: &str, parent: Option<&str>) -> ThreadRecord {
        ThreadRecord {
            id: format!("id-{name}"),
            thread: name.to_string(),
            target_thread: None,
            parent_thread: parent.map(str::to_string),
            mode: ThreadMode::Materialized,
            state: ThreadState::Active,
            base_state: "base".to_string(),
            base_root: "root".to_string(),
            current_state: None,
            merged_state: None,
            task: None,
            changed_paths: Vec::new(),
            impact_categories: Vec::new(),
            heavy_impact_paths: Vec::new(),
            promotion_suggested: false,
            freshness: ThreadFreshness::Unknown,
            verification_summary: Default::default(),
            confidence_summary: Default::default(),
            integration_policy_result: Default::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            ephemeral: None,
            auto: false,
            shared_target_dir: None,
        }
    }

    fn record_at(name: &str, parent: Option<&str>, base: &str) -> ThreadRecord {
        let mut r = record(name, parent);
        r.base_state = base.to_string();
        r
    }

    fn current_states<'a>(
        map: &'a HashMap<&'a str, &'a str>,
    ) -> impl FnMut(&str) -> String + 'a {
        move |name: &str| map.get(name).copied().unwrap_or(name).to_string()
    }

    #[test]
    fn empty_input_yields_no_stacks() {
        assert!(compute_stacks(&[]).is_empty());
    }

    #[test]
    fn single_orphan_thread_is_its_own_stack() {
        let records = vec![record("feature-a", None)];
        let stacks = compute_stacks(&records);
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].root_name(), "feature-a");
        assert_eq!(stacks[0].member_count(), 1);
        assert_eq!(stacks[0].depth(), 0);
    }

    #[test]
    fn linear_three_deep_stack() {
        let records = vec![
            record("feature-a", None),
            record("feature-b", Some("feature-a")),
            record("feature-c", Some("feature-b")),
        ];
        let stacks = compute_stacks(&records);
        assert_eq!(stacks.len(), 1);
        let stack = &stacks[0];
        assert_eq!(stack.root_name(), "feature-a");
        assert_eq!(stack.member_count(), 3);
        assert_eq!(stack.depth(), 2);
        assert_eq!(
            stack.member_names(),
            vec!["feature-a", "feature-b", "feature-c"]
        );
    }

    #[test]
    fn parent_outside_list_promotes_child_to_root() {
        let records = vec![
            record("feature-a", Some("main")),
            record("feature-b", Some("feature-a")),
        ];
        let stacks = compute_stacks(&records);
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].root_name(), "feature-a");
        assert_eq!(stacks[0].member_count(), 2);
    }

    #[test]
    fn cycle_does_not_panic_or_loop_forever() {
        let records = vec![record("a", Some("b")), record("b", Some("a"))];
        let stacks = compute_stacks(&records);
        assert!(stacks.is_empty());
    }

    #[test]
    fn stack_for_walks_up_to_root_then_returns_full_tree() {
        let records = vec![
            record("feature-a", None),
            record("feature-b", Some("feature-a")),
            record("feature-c", Some("feature-b")),
            record("feature-d", Some("feature-a")),
        ];
        let from_root = stack_for(&records, "feature-a").unwrap();
        let from_leaf = stack_for(&records, "feature-c").unwrap();
        let from_sibling = stack_for(&records, "feature-d").unwrap();
        assert_eq!(from_root, from_leaf);
        assert_eq!(from_root, from_sibling);
        assert_eq!(from_root.member_count(), 4);
    }

    #[test]
    fn stack_for_returns_none_for_unknown_thread() {
        assert!(stack_for(&[record("feature-a", None)], "missing").is_none());
    }

    #[test]
    fn plan_rejects_unknown_root() {
        let records: Vec<ThreadRecord> = Vec::new();
        let err = plan_stack_rebase(&records, "missing", "new-base", |_| String::new())
            .unwrap_err();
        assert_eq!(err, PlanRebaseError::ThreadNotFound("missing".into()));
    }

    #[test]
    fn plan_rejects_non_root_when_parent_present() {
        let records = vec![
            record_at("feature-a", None, "main-1"),
            record_at("feature-b", Some("feature-a"), "feature-a-tip"),
        ];
        let err = plan_stack_rebase(&records, "feature-b", "main-2", |n| n.to_string())
            .unwrap_err();
        assert_eq!(
            err,
            PlanRebaseError::NotARoot("feature-b".into(), "feature-a".into())
        );
    }

    #[test]
    fn plan_for_single_orphan_yields_one_step() {
        let records = vec![record_at("feature-a", None, "main-1")];
        let mut current = HashMap::new();
        current.insert("feature-a", "feature-a-tip");
        let plan = plan_stack_rebase(&records, "feature-a", "main-2", current_states(&current))
            .unwrap();
        assert_eq!(plan.step_count(), 1);
        let step = &plan.steps[0];
        assert_eq!(step.thread, "feature-a");
        assert_eq!(step.current_state, "feature-a-tip");
        assert_eq!(step.old_base, "main-1");
        assert_eq!(step.new_base, "main-2");
        assert_eq!(step.depth, 0);
        assert_eq!(step.parent_thread, None);
        assert!(!plan.is_no_op());
    }

    #[test]
    fn plan_orders_descendants_after_parents_bfs() {
        // a → { b → c, d }
        let records = vec![
            record_at("a", None, "main-1"),
            record_at("b", Some("a"), "a-tip"),
            record_at("c", Some("b"), "b-tip"),
            record_at("d", Some("a"), "a-tip"),
        ];
        let mut current = HashMap::new();
        current.insert("a", "a-tip");
        current.insert("b", "b-tip");
        current.insert("c", "c-tip");
        current.insert("d", "d-tip");
        let plan =
            plan_stack_rebase(&records, "a", "main-2", current_states(&current)).unwrap();
        let order: Vec<&str> = plan.steps.iter().map(|s| s.thread.as_str()).collect();
        assert_eq!(order, vec!["a", "b", "d", "c"]);

        let by_thread: HashMap<&str, &StackRebaseStep> =
            plan.steps.iter().map(|s| (s.thread.as_str(), s)).collect();
        assert_eq!(by_thread["a"].new_base, "main-2");
        assert_eq!(by_thread["b"].new_base, "a@projected");
        assert_eq!(by_thread["d"].new_base, "a@projected");
        assert_eq!(by_thread["c"].new_base, "b@projected");
    }

    #[test]
    fn plan_root_step_parent_thread_is_none_even_when_record_names_external_parent() {
        // The record names `main` as parent. Since `main` isn't in the
        // record set, `feat-a` is treated as a root. The emitted step's
        // `parent_thread` must be `None` regardless of what the source
        // record says — downstream root-classification keys off that
        // field, not on `depth == 0`.
        let records = vec![
            record_at("feat-a", Some("main"), "main-1"),
            record_at("feat-b", Some("feat-a"), "feat-a-tip"),
        ];
        let mut current = HashMap::new();
        current.insert("feat-a", "feat-a-tip");
        current.insert("feat-b", "feat-b-tip");
        let plan =
            plan_stack_rebase(&records, "feat-a", "main-2", current_states(&current)).unwrap();
        let by_thread: HashMap<&str, &StackRebaseStep> =
            plan.steps.iter().map(|s| (s.thread.as_str(), s)).collect();
        assert_eq!(
            by_thread["feat-a"].parent_thread, None,
            "root step must erase the record's external parent"
        );
        assert_eq!(
            by_thread["feat-b"].parent_thread,
            Some("feat-a".to_string()),
            "descendant step must retain its in-stack parent"
        );
    }

    #[test]
    fn plan_reports_no_op_when_everything_already_aligned() {
        let records = vec![record_at("a", None, "main-2")];
        let mut current = HashMap::new();
        current.insert("a", "main-2");
        let plan =
            plan_stack_rebase(&records, "a", "main-2", current_states(&current)).unwrap();
        assert!(plan.is_no_op());
    }
}
