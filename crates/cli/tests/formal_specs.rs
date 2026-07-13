//! Property-based tests derived from Quint formal specifications.
//!
//! Each test module mirrors a Quint spec in specs/quint/ and verifies
//! the same safety invariants using random command sequences.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

// =============================================================================
// Merge Resolution (mirrors specs/quint/merge_resolution.qnt)
// =============================================================================

mod merge_resolution {
    use super::*;

    #[derive(Debug, Clone)]
    struct MergeModel {
        merge_active: bool,
        conflicts: BTreeSet<&'static str>,
        resolved: BTreeSet<&'static str>,
        current_head: &'static str,
        worktree_clean: bool,
        merge_ours: &'static str,
        merge_theirs: &'static str,
    }

    impl MergeModel {
        fn init() -> Self {
            Self {
                merge_active: false,
                conflicts: BTreeSet::new(),
                resolved: BTreeSet::new(),
                current_head: "id1",
                worktree_clean: true,
                merge_ours: "",
                merge_theirs: "",
            }
        }

        fn check_invariants(&self) {
            // INV-1: resolved ⊆ conflicts
            assert!(
                self.resolved.is_subset(&self.conflicts),
                "resolved {:?} not subset of conflicts {:?}",
                self.resolved,
                self.conflicts
            );

            // INV-2: no merge → clean state
            if !self.merge_active {
                assert!(
                    self.conflicts.is_empty(),
                    "conflicts non-empty without merge"
                );
                assert!(self.resolved.is_empty(), "resolved non-empty without merge");
            }

            // INV-3: merge parents consistent
            if self.merge_active {
                assert!(!self.merge_ours.is_empty(), "merge_ours empty during merge");
                assert!(
                    !self.merge_theirs.is_empty(),
                    "merge_theirs empty during merge"
                );
            } else {
                assert!(self.merge_ours.is_empty(), "merge_ours set without merge");
                assert!(
                    self.merge_theirs.is_empty(),
                    "merge_theirs set without merge"
                );
            }

            // INV-4: merge active → worktree dirty
            if self.merge_active {
                assert!(!self.worktree_clean, "worktree clean during merge");
            }
        }

        fn unresolved(&self) -> BTreeSet<&'static str> {
            self.conflicts.difference(&self.resolved).copied().collect()
        }

        fn apply(&mut self, cmd: &MergeCommand) {
            match cmd {
                MergeCommand::StartMerge { conflict_files } => {
                    if self.merge_active || !self.worktree_clean {
                        return; // precondition not met
                    }
                    if conflict_files.is_empty() {
                        // Clean merge: auto-commit
                        self.current_head = "id_merged";
                    } else {
                        self.merge_active = true;
                        self.conflicts = conflict_files.iter().copied().collect();
                        self.resolved = BTreeSet::new();
                        self.merge_ours = self.current_head;
                        self.merge_theirs = "id_theirs";
                        self.worktree_clean = false;
                    }
                }
                MergeCommand::ResolveFile(path) => {
                    if !self.merge_active
                        || !self.conflicts.contains(path)
                        || self.resolved.contains(path)
                    {
                        return;
                    }
                    self.resolved.insert(path);
                }
                MergeCommand::ResolveAll => {
                    if !self.merge_active || self.unresolved().is_empty() {
                        return;
                    }
                    self.resolved = self.conflicts.clone();
                }
                MergeCommand::Abort => {
                    if !self.merge_active {
                        return;
                    }
                    self.merge_active = false;
                    self.conflicts.clear();
                    self.resolved.clear();
                    self.worktree_clean = true;
                    self.merge_ours = "";
                    self.merge_theirs = "";
                }
                MergeCommand::Finish => {
                    if !self.merge_active || !self.unresolved().is_empty() {
                        return;
                    }
                    self.current_head = "id_merge_result";
                    self.merge_active = false;
                    self.conflicts.clear();
                    self.resolved.clear();
                    self.worktree_clean = true;
                    self.merge_ours = "";
                    self.merge_theirs = "";
                }
                MergeCommand::Snapshot => {
                    if self.merge_active {
                        if !self.unresolved().is_empty() {
                            return; // cannot snapshot with unresolved conflicts
                        }
                        // Merge commit
                        self.current_head = "id_merge_result";
                        self.merge_active = false;
                        self.conflicts.clear();
                        self.resolved.clear();
                        self.worktree_clean = true;
                        self.merge_ours = "";
                        self.merge_theirs = "";
                    } else {
                        if self.worktree_clean {
                            return;
                        }
                        self.current_head = "id_new";
                        self.worktree_clean = true;
                    }
                }
                MergeCommand::ModifyWorktree => {
                    if self.merge_active || !self.worktree_clean {
                        return;
                    }
                    self.worktree_clean = false;
                }
            }
        }
    }

    const FILES: [&str; 4] = ["a.rs", "b.rs", "c.rs", "d.rs"];

    #[derive(Debug, Clone)]
    enum MergeCommand {
        StartMerge { conflict_files: Vec<&'static str> },
        ResolveFile(&'static str),
        ResolveAll,
        Abort,
        Finish,
        Snapshot,
        ModifyWorktree,
    }

    fn arb_merge_command() -> impl Strategy<Value = MergeCommand> {
        prop_oneof![
            // StartMerge with random subset of files as conflicts
            proptest::sample::subsequence(&FILES, 0..=4).prop_map(|files| {
                MergeCommand::StartMerge {
                    conflict_files: files,
                }
            }),
            // ResolveFile with random file
            (0..4usize).prop_map(|i| MergeCommand::ResolveFile(FILES[i])),
            Just(MergeCommand::ResolveAll),
            Just(MergeCommand::Abort),
            Just(MergeCommand::Finish),
            Just(MergeCommand::Snapshot),
            Just(MergeCommand::ModifyWorktree),
        ]
    }

    proptest! {
        #[test]
        fn merge_safety_invariants(
            commands in proptest::collection::vec(arb_merge_command(), 1..50)
        ) {
            let mut model = MergeModel::init();
            model.check_invariants();
            for cmd in &commands {
                model.apply(cmd);
                model.check_invariants();
            }
        }
    }
}

// =============================================================================
// Lock Protocol (mirrors specs/quint/lock_protocol.qnt)
// =============================================================================

mod lock_protocol {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum LockMode {
        None,
        Read,
        Write,
    }

    #[derive(Debug, Clone)]
    struct LockModel {
        // (process, lock) -> mode
        holding: BTreeMap<(&'static str, &'static str), LockMode>,
        readers: BTreeMap<&'static str, i32>,
        writer_held: BTreeMap<&'static str, bool>,
    }

    const PROCS: [&str; 3] = ["p1", "p2", "p3"];
    const LOCKS: [&str; 2] = ["repo", "refs"];

    impl LockModel {
        fn init() -> Self {
            let mut holding = BTreeMap::new();
            let mut readers = BTreeMap::new();
            let mut writer_held = BTreeMap::new();
            for p in &PROCS {
                for l in &LOCKS {
                    holding.insert((*p, *l), LockMode::None);
                }
            }
            for l in &LOCKS {
                readers.insert(*l, 0);
                writer_held.insert(*l, false);
            }
            Self {
                holding,
                readers,
                writer_held,
            }
        }

        fn check_invariants(&self) {
            for lock in &LOCKS {
                // INV-1: Mutual exclusion
                if self.writer_held[lock] {
                    assert_eq!(
                        self.readers[lock], 0,
                        "writer held but {} readers on {}",
                        self.readers[lock], lock
                    );
                }

                // INV-2: Single writer
                let writers: Vec<_> = PROCS
                    .iter()
                    .filter(|p| self.holding[&(**p, *lock)] == LockMode::Write)
                    .collect();
                if self.writer_held[lock] {
                    assert_eq!(writers.len(), 1, "multiple writers on {}", lock);
                } else {
                    assert_eq!(writers.len(), 0, "writer count mismatch on {}", lock);
                }

                // INV-3: Reader count consistent
                let actual_readers = PROCS
                    .iter()
                    .filter(|p| self.holding[&(**p, *lock)] == LockMode::Read)
                    .count() as i32;
                assert_eq!(
                    self.readers[lock], actual_readers,
                    "reader count mismatch on {}",
                    lock
                );

                // INV-4: No negative readers
                assert!(self.readers[lock] >= 0, "negative readers on {}", lock);
            }

            // INV-5: Lock ordering — write on refs requires holding repo
            for proc in &PROCS {
                if self.holding[&(*proc, "refs")] == LockMode::Write {
                    assert_ne!(
                        self.holding[&(*proc, "repo")],
                        LockMode::None,
                        "{} holds write on refs without repo",
                        proc
                    );
                }
            }
        }

        fn apply(&mut self, cmd: &LockCommand) {
            match cmd {
                LockCommand::AcquireRead(proc, lock) => {
                    if self.writer_held[lock] {
                        return;
                    }
                    if self.holding[&(*proc, *lock)] != LockMode::None {
                        return;
                    }
                    *self.readers.get_mut(lock).unwrap() += 1;
                    self.holding.insert((*proc, *lock), LockMode::Read);
                }
                LockCommand::AcquireWrite(proc, lock) => {
                    if self.readers[lock] != 0 || self.writer_held[lock] {
                        return;
                    }
                    if self.holding[&(*proc, *lock)] != LockMode::None {
                        return;
                    }
                    // Lock ordering: write on refs requires holding repo
                    if *lock == "refs" && self.holding[&(*proc, "repo")] == LockMode::None {
                        return;
                    }
                    self.writer_held.insert(*lock, true);
                    self.holding.insert((*proc, *lock), LockMode::Write);
                }
                LockCommand::Release(proc, lock) => {
                    if self.holding[&(*proc, *lock)] == LockMode::None {
                        return;
                    }
                    // Release ordering: can't release repo while holding refs
                    if *lock == "repo" && self.holding[&(*proc, "refs")] != LockMode::None {
                        return;
                    }
                    match self.holding[&(*proc, *lock)] {
                        LockMode::Read => {
                            *self.readers.get_mut(lock).unwrap() -= 1;
                        }
                        LockMode::Write => {
                            self.writer_held.insert(*lock, false);
                        }
                        LockMode::None => {}
                    }
                    self.holding.insert((*proc, *lock), LockMode::None);
                }
            }
        }
    }

    #[derive(Debug, Clone)]
    enum LockCommand {
        AcquireRead(&'static str, &'static str),
        AcquireWrite(&'static str, &'static str),
        Release(&'static str, &'static str),
    }

    fn arb_lock_command() -> impl Strategy<Value = LockCommand> {
        let proc_idx = 0..3usize;
        let lock_idx = 0..2usize;
        (proc_idx, lock_idx, 0..3u8).prop_map(|(pi, li, action)| {
            let proc = PROCS[pi];
            let lock = LOCKS[li];
            match action {
                0 => LockCommand::AcquireRead(proc, lock),
                1 => LockCommand::AcquireWrite(proc, lock),
                _ => LockCommand::Release(proc, lock),
            }
        })
    }

    proptest! {
        #[test]
        fn lock_safety_invariants(
            commands in proptest::collection::vec(arb_lock_command(), 1..100)
        ) {
            let mut model = LockModel::init();
            model.check_invariants();
            for cmd in &commands {
                model.apply(cmd);
                model.check_invariants();
            }
        }
    }
}

// =============================================================================
// Agent Lifecycle (mirrors specs/quint/agent_lifecycle.qnt)
// =============================================================================

mod agent_lifecycle {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Status {
        Active,
        Complete,
        Merged,
    }

    #[derive(Debug, Clone)]
    struct ActorPresence {
        status: Status,
        completed_at: Option<i32>,
    }

    #[derive(Debug, Clone)]
    struct AgentModel {
        agents: BTreeMap<&'static str, ActorPresence>,
        current_time: i32,
    }

    const STALE_TTL: i32 = 7;
    const SESSIONS: [&str; 3] = ["agent-1", "agent-2", "agent-3"];

    impl AgentModel {
        fn init() -> Self {
            Self {
                agents: BTreeMap::new(),
                current_time: 0,
            }
        }

        fn check_invariants(&self) {
            for (id, entry) in &self.agents {
                // INV-1: No backward transitions (structural — guards prevent it)
                // INV-2: completedAt consistent with status
                match entry.status {
                    Status::Active => {
                        assert!(
                            entry.completed_at.is_none(),
                            "{}: active but has completed_at",
                            id
                        );
                    }
                    Status::Complete | Status::Merged => {
                        assert!(
                            entry.completed_at.is_some(),
                            "{}: terminal but no completed_at",
                            id
                        );
                    }
                }
            }
        }

        fn apply(&mut self, cmd: &AgentCommand) {
            match cmd {
                AgentCommand::Spawn(id) => {
                    if self.agents.contains_key(id) {
                        return;
                    }
                    self.agents.insert(
                        id,
                        ActorPresence {
                            status: Status::Active,
                            completed_at: None,
                        },
                    );
                }
                AgentCommand::Done(id) => {
                    if let Some(entry) = self.agents.get_mut(id) {
                        if entry.status != Status::Active {
                            return;
                        }
                        entry.status = Status::Complete;
                        entry.completed_at = Some(self.current_time);
                    }
                }
                AgentCommand::Merge(id) => {
                    if let Some(entry) = self.agents.get_mut(id) {
                        if entry.status == Status::Merged {
                            return;
                        }
                        if entry.completed_at.is_none() {
                            entry.completed_at = Some(self.current_time);
                        }
                        entry.status = Status::Merged;
                    }
                }
                AgentCommand::ListAndPrune => {
                    let stale_ids: Vec<&'static str> = self
                        .agents
                        .iter()
                        .filter(|(_, e)| {
                            e.status != Status::Active
                                && e.completed_at
                                    .map(|t| self.current_time - t >= STALE_TTL)
                                    .unwrap_or(false)
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    for id in stale_ids {
                        self.agents.remove(id);
                    }
                }
                AgentCommand::AdvanceTime => {
                    self.current_time += 1;
                }
            }
        }
    }

    #[derive(Debug, Clone)]
    enum AgentCommand {
        Spawn(&'static str),
        Done(&'static str),
        Merge(&'static str),
        ListAndPrune,
        AdvanceTime,
    }

    fn arb_agent_command() -> impl Strategy<Value = AgentCommand> {
        prop_oneof![
            (0..3usize).prop_map(|i| AgentCommand::Spawn(SESSIONS[i])),
            (0..3usize).prop_map(|i| AgentCommand::Done(SESSIONS[i])),
            (0..3usize).prop_map(|i| AgentCommand::Merge(SESSIONS[i])),
            Just(AgentCommand::ListAndPrune),
            Just(AgentCommand::AdvanceTime),
        ]
    }

    proptest! {
        #[test]
        fn agent_safety_invariants(
            commands in proptest::collection::vec(arb_agent_command(), 1..50)
        ) {
            let mut model = AgentModel::init();
            model.check_invariants();
            for cmd in &commands {
                model.apply(cmd);
                model.check_invariants();
            }
        }
    }
}

// =============================================================================
// Worktree Lifecycle (mirrors specs/quint/worktree_lifecycle.qnt)
// =============================================================================

mod worktree_lifecycle {
    use super::*;

    #[derive(Debug, Clone)]
    struct Worktree {
        name: &'static str,
        dirty: bool,
    }

    #[derive(Debug, Clone)]
    struct WorktreeModel {
        worktrees: BTreeMap<&'static str, Worktree>,
        current: &'static str,
    }

    const WT_IDS: [&str; 3] = ["wt-1", "wt-2", "wt-3"];
    const WT_NAMES: [&str; 3] = ["default", "feature", "bugfix"];

    impl WorktreeModel {
        fn init() -> Self {
            let mut worktrees = BTreeMap::new();
            worktrees.insert(
                "wt-1",
                Worktree {
                    name: "default",
                    dirty: false,
                },
            );
            Self {
                worktrees,
                current: "wt-1",
            }
        }

        fn check_invariants(&self) {
            // INV-1: Current worktree exists
            assert!(
                self.worktrees.contains_key(self.current),
                "current {} not in registry",
                self.current
            );

            // INV-2: Unique names
            let names: Vec<_> = self.worktrees.values().map(|w| w.name).collect();
            let unique: BTreeSet<_> = names.iter().collect();
            assert_eq!(names.len(), unique.len(), "duplicate worktree names");

            // INV-3: At least one worktree
            assert!(!self.worktrees.is_empty(), "no worktrees in registry");
        }

        fn name_in_use(&self, name: &str) -> bool {
            self.worktrees.values().any(|w| w.name == name)
        }

        fn apply(&mut self, cmd: &WorktreeCommand) {
            match cmd {
                WorktreeCommand::Create(id, name) => {
                    if self.worktrees.contains_key(id) || self.name_in_use(name) {
                        return;
                    }
                    self.worktrees.insert(id, Worktree { name, dirty: false });
                }
                WorktreeCommand::Switch(target) => {
                    if !self.worktrees.contains_key(target) || *target == self.current {
                        return;
                    }
                    self.current = target;
                    // Materialized worktree is clean
                    self.worktrees.get_mut(target).unwrap().dirty = false;
                }
                WorktreeCommand::Delete(id, force) => {
                    // CANNOT delete current worktree
                    if *id == self.current {
                        return;
                    }
                    if let Some(wt) = self.worktrees.get(id) {
                        if !force && wt.dirty {
                            return;
                        }
                        self.worktrees.remove(id);
                    }
                }
                WorktreeCommand::Modify => {
                    if let Some(wt) = self.worktrees.get_mut(self.current) {
                        wt.dirty = true;
                    }
                }
                WorktreeCommand::Snapshot => {
                    if let Some(wt) = self.worktrees.get_mut(self.current)
                        && wt.dirty
                    {
                        wt.dirty = false;
                    }
                }
            }
        }
    }

    #[derive(Debug, Clone)]
    enum WorktreeCommand {
        Create(&'static str, &'static str),
        Switch(&'static str),
        Delete(&'static str, bool),
        Modify,
        Snapshot,
    }

    fn arb_worktree_command() -> impl Strategy<Value = WorktreeCommand> {
        prop_oneof![
            (0..3usize, 0..3usize)
                .prop_map(|(id, name)| WorktreeCommand::Create(WT_IDS[id], WT_NAMES[name])),
            (0..3usize).prop_map(|i| WorktreeCommand::Switch(WT_IDS[i])),
            (0..3usize, proptest::bool::ANY)
                .prop_map(|(i, force)| WorktreeCommand::Delete(WT_IDS[i], force)),
            Just(WorktreeCommand::Modify),
            Just(WorktreeCommand::Snapshot),
        ]
    }

    proptest! {
        #[test]
        fn worktree_safety_invariants(
            commands in proptest::collection::vec(arb_worktree_command(), 1..50)
        ) {
            let mut model = WorktreeModel::init();
            model.check_invariants();
            for cmd in &commands {
                model.apply(cmd);
                model.check_invariants();
            }
        }
    }
}

// =============================================================================
// Repository Ops (mirrors specs/quint/repository_ops.qnt)
// =============================================================================

mod repository_ops {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum HeadState {
        Attached(String),
        Detached(String),
    }

    #[derive(Debug, Clone)]
    struct RepoModel {
        head: HeadState,
        threads: BTreeMap<String, String>,
        worktree_clean: bool,
        merge_in_progress: bool,
        merge_conflicts: BTreeSet<String>,
        merge_resolved: BTreeSet<String>,
    }

    const THREAD_NAMES: [&str; 2] = ["main", "feat"];
    const CHANGE_IDS: [&str; 5] = ["id1", "id2", "id3", "id4", "id5"];
    const FILES: [&str; 4] = ["a.rs", "b.rs", "c.rs", "d.rs"];

    impl RepoModel {
        fn init() -> Self {
            let mut threads = BTreeMap::new();
            threads.insert("main".to_string(), "id1".to_string());
            threads.insert("feat".to_string(), "id2".to_string());
            Self {
                head: HeadState::Attached("main".to_string()),
                threads,
                worktree_clean: true,
                merge_in_progress: false,
                merge_conflicts: BTreeSet::new(),
                merge_resolved: BTreeSet::new(),
            }
        }

        fn unresolved(&self) -> BTreeSet<&String> {
            self.merge_conflicts
                .difference(&self.merge_resolved)
                .collect()
        }

        fn check_invariants(&self) {
            // INV-1: resolved ⊆ conflicts
            assert!(self.merge_resolved.is_subset(&self.merge_conflicts));

            // INV-2: clean when no merge
            if !self.merge_in_progress {
                assert!(self.merge_conflicts.is_empty());
                assert!(self.merge_resolved.is_empty());
            }

            // INV-3: attached head references existing thread
            if let HeadState::Attached(ref thread) = self.head {
                assert!(
                    self.threads.contains_key(thread),
                    "HEAD attached to non-existent thread {}",
                    thread
                );
            }
        }

        fn apply(&mut self, cmd: &RepoCommand) {
            match cmd {
                RepoCommand::Snapshot(new_id) => {
                    if self.merge_in_progress {
                        if !self.unresolved().is_empty() {
                            return; // cannot snapshot with unresolved conflicts
                        }
                    } else if self.worktree_clean {
                        return;
                    }
                    match &self.head {
                        HeadState::Attached(thread) => {
                            self.threads.insert(thread.clone(), new_id.to_string());
                        }
                        HeadState::Detached(_) => {
                            self.head = HeadState::Detached(new_id.to_string());
                        }
                    }
                    self.worktree_clean = true;
                    self.merge_in_progress = false;
                    self.merge_conflicts.clear();
                    self.merge_resolved.clear();
                }
                RepoCommand::Goto(target_id) => {
                    if self.merge_in_progress {
                        return;
                    }
                    self.head = HeadState::Detached(target_id.to_string());
                    self.worktree_clean = true;
                }
                RepoCommand::StartMerge(thread, conflict_indices) => {
                    if self.merge_in_progress || !self.worktree_clean {
                        return;
                    }
                    if !self.threads.contains_key(*thread) {
                        return;
                    }
                    let conflict_files: BTreeSet<String> = conflict_indices
                        .iter()
                        .map(|i| FILES[*i].to_string())
                        .collect();
                    if conflict_files.is_empty() {
                        // Clean merge
                        match &self.head {
                            HeadState::Attached(t) => {
                                self.threads.insert(t.clone(), "id_merged".to_string());
                            }
                            HeadState::Detached(_) => {
                                self.head = HeadState::Detached("id_merged".to_string());
                            }
                        }
                    } else {
                        self.merge_in_progress = true;
                        self.merge_conflicts = conflict_files;
                        self.merge_resolved.clear();
                        self.worktree_clean = false;
                    }
                }
                RepoCommand::ResolveConflict(file_idx) => {
                    let file = FILES[*file_idx].to_string();
                    if !self.merge_in_progress
                        || !self.merge_conflicts.contains(&file)
                        || self.merge_resolved.contains(&file)
                    {
                        return;
                    }
                    self.merge_resolved.insert(file);
                }
                RepoCommand::AbortMerge => {
                    if !self.merge_in_progress {
                        return;
                    }
                    self.merge_in_progress = false;
                    self.merge_conflicts.clear();
                    self.merge_resolved.clear();
                    self.worktree_clean = true;
                }
                RepoCommand::ModifyWorktree => {
                    if self.merge_in_progress || !self.worktree_clean {
                        return;
                    }
                    self.worktree_clean = false;
                }
                RepoCommand::AttachHead(thread) => {
                    if self.merge_in_progress || !self.threads.contains_key(*thread) {
                        return;
                    }
                    self.head = HeadState::Attached(thread.to_string());
                }
            }
        }
    }

    #[derive(Debug, Clone)]
    enum RepoCommand {
        Snapshot(&'static str),
        Goto(&'static str),
        StartMerge(&'static str, Vec<usize>),
        ResolveConflict(usize),
        AbortMerge,
        ModifyWorktree,
        AttachHead(&'static str),
    }

    fn arb_repo_command() -> impl Strategy<Value = RepoCommand> {
        prop_oneof![
            (0..5usize).prop_map(|i| RepoCommand::Snapshot(CHANGE_IDS[i])),
            (0..5usize).prop_map(|i| RepoCommand::Goto(CHANGE_IDS[i])),
            (
                0..2usize,
                proptest::sample::subsequence(&[0usize, 1, 2, 3], 0..=4)
            )
                .prop_map(|(t, files)| RepoCommand::StartMerge(THREAD_NAMES[t], files)),
            (0..4usize).prop_map(RepoCommand::ResolveConflict),
            Just(RepoCommand::AbortMerge),
            Just(RepoCommand::ModifyWorktree),
            (0..2usize).prop_map(|i| RepoCommand::AttachHead(THREAD_NAMES[i])),
        ]
    }

    proptest! {
        #[test]
        fn repo_ops_safety_invariants(
            commands in proptest::collection::vec(arb_repo_command(), 1..50)
        ) {
            let mut model = RepoModel::init();
            model.check_invariants();
            for cmd in &commands {
                model.apply(cmd);
                model.check_invariants();
            }
        }
    }
}
