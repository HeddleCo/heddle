// SPDX-License-Identifier: Apache-2.0
//! Pure agent fanout planning and plan dry-run report assembly.
//!
//! Owns:
//! - lane string parse (`thread=path:title`) and thread-name validation
//! - pure empty-lane / duplicate-thread preflight
//! - parent-thread label selection from HEAD attach facts
//! - parent/child task body and delegated-by string rules
//! - dry-run `agent fanout start` command argv assembly
//! - plan dry-run report fields (stable JSON names, no verification)
//!
//! Worktree target resolution, agent-task store writes, thread spawn, and
//! registry linking stay CLI-owned. Live-reservation / existing-thread /
//! path-collision **decisions** after the CLI gathers facts are pure here.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use repo::{ThreadId, ThreadIdError, shell_quote};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One child lane after successful parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanoutNodeSpec {
    pub thread: String,
    pub path: PathBuf,
    pub title: String,
}

/// CLI-gathered availability facts for one fanout lane start preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanoutLaneAvailability {
    pub thread: String,
    /// Active agent reservation already owns this thread name.
    pub has_live_owner: bool,
    /// Thread ref already exists in the repository.
    pub thread_ref_exists: bool,
    /// Active thread record (ThreadManager) already exists.
    pub active_thread_record: bool,
    /// Resolved absolute/normalized checkout path for collision detection.
    pub resolved_path: PathBuf,
}

/// Pure preflight refusal after live I/O facts are known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FanoutLanePreflightBlock {
    LiveOwner { thread: String },
    ThreadExists { thread: String },
    ActiveThreadRecord { thread: String },
    DuplicatePath { thread: String },
}

impl FanoutLanePreflightBlock {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::LiveOwner { .. } => "agent_fanout_live_owner",
            Self::ThreadExists { .. } | Self::ActiveThreadRecord { .. } => {
                "agent_fanout_thread_exists"
            }
            Self::DuplicatePath { .. } => "agent_fanout_duplicate_path",
        }
    }

    pub fn thread(&self) -> &str {
        match self {
            Self::LiveOwner { thread }
            | Self::ThreadExists { thread }
            | Self::ActiveThreadRecord { thread }
            | Self::DuplicatePath { thread } => thread,
        }
    }
}

/// Pure fanout-start preflight from CLI-gathered per-lane facts.
///
/// Checks live owner, existing thread ref/record, then path collisions across
/// the lane set (in order).
pub fn check_fanout_start_preflight(
    lanes: &[FanoutLaneAvailability],
) -> Result<(), FanoutLanePreflightBlock> {
    let mut seen_paths = BTreeSet::new();
    for lane in lanes {
        if lane.has_live_owner {
            return Err(FanoutLanePreflightBlock::LiveOwner {
                thread: lane.thread.clone(),
            });
        }
        if lane.thread_ref_exists {
            return Err(FanoutLanePreflightBlock::ThreadExists {
                thread: lane.thread.clone(),
            });
        }
        if lane.active_thread_record {
            return Err(FanoutLanePreflightBlock::ActiveThreadRecord {
                thread: lane.thread.clone(),
            });
        }
        if !seen_paths.insert(lane.resolved_path.clone()) {
            return Err(FanoutLanePreflightBlock::DuplicatePath {
                thread: lane.thread.clone(),
            });
        }
    }
    Ok(())
}

impl FanoutNodeSpec {
    /// Reconstruct the CLI `--lane` value form: `thread=path:title`.
    pub fn to_lane_arg(&self) -> String {
        format!("{}={}:{}", self.thread, self.path.display(), self.title)
    }
}

/// Caller-supplied fanout plan/start inputs (plus resolved base facts).
///
/// Field names mirror the CLI `agent fanout plan|start` surface. HEAD resolution
/// and repository I/O stay caller-owned; pass already-resolved base fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanoutPlanRequest {
    pub title: String,
    /// Raw `--lane` values: `thread=path:title`.
    pub lanes: Vec<String>,
    pub coordination_discussion_id: Option<String>,
    /// Full HEAD state id (caller-resolved).
    pub base_state: String,
    /// Short tree/root id for the HEAD state (caller-resolved).
    pub base_root: String,
    /// Parent coordination thread name from HEAD (or `"detached"`).
    pub parent_thread: String,
}

/// Resolved base facts the CLI gathers from HEAD before planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanoutBaseFacts {
    /// Full HEAD state id string.
    pub head_state_full: String,
    /// Short tree id of the HEAD state.
    pub head_tree_short: String,
    /// Attached thread name when HEAD is attached; `None` when detached.
    pub head_thread: Option<String>,
}

/// Selected base for fanout parent/child tasks and lane starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanoutBaseSelection {
    pub base_state: String,
    pub base_root: String,
    pub parent_thread: String,
}

/// Pure plan produced by [`plan_fanout`].
///
/// CLI executes task creation, thread spawn, and registry linking from `nodes`
/// and the body/delegated-by helpers. Dry-run commands are for plan JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanoutPlan {
    pub title: String,
    pub parent_thread: String,
    pub base_state: String,
    pub base_root: String,
    pub coordination_discussion_id: Option<String>,
    pub nodes: Vec<FanoutNodeSpec>,
    /// Parent coordination task body (bullet list of lanes).
    pub parent_body: String,
    /// Dry-run start commands for plan JSON (`commands` field).
    pub start_commands: Vec<FanoutCommandSpec>,
}

/// One recommended command in fanout plan JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FanoutCommandSpec {
    pub lane_thread: String,
    pub command: String,
    pub argv: Vec<String>,
}

/// Machine JSON domain fields for `agent fanout plan` (no verification envelope).
///
/// Field names match the public `agent_fanout_plan` contract. `parent_task` and
/// per-lane `task` stay `null` for dry-run; CLI attaches verification.
#[derive(Debug, Clone, Serialize)]
pub struct FanoutPlanReport {
    pub output_kind: &'static str,
    pub title: String,
    pub parent_thread: String,
    pub base_state: String,
    pub base_root: String,
    pub coordination_discussion_id: Option<String>,
    pub parent_task: Option<FanoutTaskPlaceholder>,
    pub lanes: Vec<FanoutLaneReport>,
    pub commands: Vec<FanoutCommandSpec>,
}

/// Placeholder so plan JSON keeps a null `parent_task` / lane `task` slot.
///
/// Never constructed for dry-run; exists only for typed `Option` serialization.
#[derive(Debug, Clone, Serialize)]
pub struct FanoutTaskPlaceholder {}

/// One planned lane in dry-run JSON (`status = "planned"`).
#[derive(Debug, Clone, Serialize)]
pub struct FanoutLaneReport {
    pub thread: String,
    pub path: String,
    pub title: String,
    pub task: Option<FanoutTaskPlaceholder>,
    pub session_id: Option<String>,
    pub status: String,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failures from pure fanout planning / lane parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FanoutPlanError {
    /// No `--lane` values were supplied.
    LaneRequired,
    /// A lane string was not `thread=path:title` with non-empty path/title.
    LaneInvalid { raw: String },
    /// Thread name failed the safe-slug / reserved-structure rule.
    InvalidThreadName { raw: String, source: ThreadIdError },
    /// The same child thread name appeared more than once.
    DuplicateThread { thread: String },
}

impl std::fmt::Display for FanoutPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LaneRequired => write!(
                f,
                "agent fanout requires at least one --lane <thread>=<path>:<title>"
            ),
            Self::LaneInvalid { raw } => write!(f, "invalid fanout lane '{raw}'"),
            Self::InvalidThreadName { source, .. } => write!(f, "{source}"),
            Self::DuplicateThread { thread } => {
                write!(f, "fanout lane '{thread}' is listed more than once")
            }
        }
    }
}

impl std::error::Error for FanoutPlanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidThreadName { source, .. } => Some(source),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Base selection / naming rules
// ---------------------------------------------------------------------------

/// Parent-thread label for fanout when HEAD attach facts are already known.
///
/// Detached HEAD uses the stable label `"detached"` (matches CLI).
pub fn select_fanout_parent_thread(head_thread: Option<&str>) -> String {
    match head_thread.map(str::trim).filter(|s| !s.is_empty()) {
        Some(thread) => thread.to_string(),
        None => "detached".to_string(),
    }
}

/// Select fanout base state / root / parent thread from caller-resolved HEAD facts.
///
/// Fanout always anchors children at the current HEAD state (no `--from`).
pub fn select_fanout_base(facts: &FanoutBaseFacts) -> FanoutBaseSelection {
    FanoutBaseSelection {
        base_state: facts.head_state_full.clone(),
        base_root: facts.head_tree_short.clone(),
        parent_thread: select_fanout_parent_thread(facts.head_thread.as_deref()),
    }
}

/// Parent coordination task body: one bullet per lane (`- thread: title`).
pub fn fanout_parent_body(nodes: &[FanoutNodeSpec]) -> String {
    nodes
        .iter()
        .map(|node| format!("- {}: {}", node.thread, node.title))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Child task body linking back to the parent task id.
pub fn fanout_child_body(parent_task_id: &str) -> String {
    format!("Fan-out child lane for parent task {parent_task_id}")
}

/// `delegated_by` value written on the parent coordination task at start.
pub fn fanout_parent_delegated_by() -> &'static str {
    "heddle agent fanout start"
}

/// Attach-precedence / winning-attach-rule token for fanout-started reservations.
pub fn fanout_start_attach_rule() -> &'static str {
    "agent-fanout-start"
}

// ---------------------------------------------------------------------------
// Lane parse
// ---------------------------------------------------------------------------

/// Parse a single `--lane` value: `thread=path:title`.
pub fn parse_fanout_lane(raw: &str) -> Result<FanoutNodeSpec, FanoutPlanError> {
    let (thread, rest) = raw
        .split_once('=')
        .ok_or_else(|| FanoutPlanError::LaneInvalid {
            raw: raw.to_string(),
        })?;
    let (path, title) = rest
        .split_once(':')
        .ok_or_else(|| FanoutPlanError::LaneInvalid {
            raw: raw.to_string(),
        })?;
    let thread = thread.trim();
    let path = path.trim();
    let title = title.trim();
    if path.is_empty() || title.is_empty() {
        return Err(FanoutPlanError::LaneInvalid {
            raw: raw.to_string(),
        });
    }
    let validated = ThreadId::new(thread).map_err(|source| FanoutPlanError::InvalidThreadName {
        raw: raw.to_string(),
        source,
    })?;
    Ok(FanoutNodeSpec {
        thread: validated.as_str().to_string(),
        path: PathBuf::from(path),
        title: title.to_string(),
    })
}

/// Parse all `--lane` values; requires at least one.
pub fn parse_fanout_lanes(raw_lanes: &[String]) -> Result<Vec<FanoutNodeSpec>, FanoutPlanError> {
    if raw_lanes.is_empty() {
        return Err(FanoutPlanError::LaneRequired);
    }
    raw_lanes.iter().map(|raw| parse_fanout_lane(raw)).collect()
}

// ---------------------------------------------------------------------------
// Plan / report
// ---------------------------------------------------------------------------

/// Pure preflight: parse lanes, reject duplicate thread names, assemble plan.
///
/// Does not open the repository or touch the filesystem. CLI still runs I/O
/// preflight (live reservations, existing threads, resolved path collisions)
/// before start mutations.
pub fn plan_fanout(request: &FanoutPlanRequest) -> Result<FanoutPlan, FanoutPlanError> {
    let nodes = parse_fanout_lanes(&request.lanes)?;
    ensure_unique_thread_names(&nodes)?;
    let parent_body = fanout_parent_body(&nodes);
    let start_commands = assemble_fanout_start_commands(
        &request.title,
        request.coordination_discussion_id.as_deref(),
        &nodes,
    );
    Ok(FanoutPlan {
        title: request.title.clone(),
        parent_thread: request.parent_thread.clone(),
        base_state: request.base_state.clone(),
        base_root: request.base_root.clone(),
        coordination_discussion_id: request.coordination_discussion_id.clone(),
        nodes,
        parent_body,
        start_commands,
    })
}

/// Reject duplicate child thread names within one fanout.
pub fn ensure_unique_thread_names(nodes: &[FanoutNodeSpec]) -> Result<(), FanoutPlanError> {
    let mut seen = BTreeSet::new();
    for node in nodes {
        if !seen.insert(node.thread.as_str()) {
            return Err(FanoutPlanError::DuplicateThread {
                thread: node.thread.clone(),
            });
        }
    }
    Ok(())
}

/// Build dry-run `heddle agent fanout start ...` argv + shell-quoted command.
pub fn assemble_fanout_start_commands(
    title: &str,
    coordination_discussion_id: Option<&str>,
    nodes: &[FanoutNodeSpec],
) -> Vec<FanoutCommandSpec> {
    let mut argv = vec![
        "heddle".to_string(),
        "agent".to_string(),
        "fanout".to_string(),
        "start".to_string(),
        "--title".to_string(),
        title.to_string(),
    ];
    if let Some(discussion_id) = coordination_discussion_id {
        argv.push("--coordination-discussion-id".to_string());
        argv.push(discussion_id.to_string());
    }
    for node in nodes {
        argv.push("--lane".to_string());
        argv.push(node.to_lane_arg());
    }
    let command = argv
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    vec![FanoutCommandSpec {
        lane_thread: "all".to_string(),
        command,
        argv,
    }]
}

/// Assemble plan dry-run report fields from a pure plan.
pub fn assemble_fanout_plan_report(plan: &FanoutPlan) -> FanoutPlanReport {
    FanoutPlanReport {
        output_kind: "agent_fanout_plan",
        title: plan.title.clone(),
        parent_thread: plan.parent_thread.clone(),
        base_state: plan.base_state.clone(),
        base_root: plan.base_root.clone(),
        coordination_discussion_id: plan.coordination_discussion_id.clone(),
        parent_task: None,
        lanes: plan
            .nodes
            .iter()
            .map(|node| FanoutLaneReport {
                thread: node.thread.clone(),
                path: path_display(&node.path),
                title: node.title.clone(),
                task: None,
                session_id: None,
                status: "planned".to_string(),
            })
            .collect(),
        commands: plan.start_commands.clone(),
    }
}

fn path_display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request(lanes: &[&str]) -> FanoutPlanRequest {
        FanoutPlanRequest {
            title: "Coordinate fanout".to_string(),
            lanes: lanes.iter().map(|s| (*s).to_string()).collect(),
            coordination_discussion_id: Some("discussion-123".to_string()),
            base_state: "state-full".to_string(),
            base_root: "tree-short".to_string(),
            parent_thread: "main".to_string(),
        }
    }

    #[test]
    fn parse_lane_accepts_thread_path_title() {
        let node = parse_fanout_lane("feature/a=../a:Implement a").unwrap();
        assert_eq!(node.thread, "feature/a");
        assert_eq!(node.path, PathBuf::from("../a"));
        assert_eq!(node.title, "Implement a");
        assert_eq!(node.to_lane_arg(), "feature/a=../a:Implement a");
    }

    #[test]
    fn parse_lane_trims_whitespace() {
        let node = parse_fanout_lane("  feature/b = ./b : Title B  ").unwrap();
        assert_eq!(node.thread, "feature/b");
        assert_eq!(node.path, PathBuf::from("./b"));
        assert_eq!(node.title, "Title B");
    }

    #[test]
    fn parse_lane_rejects_missing_separators_and_empty_parts() {
        assert!(matches!(
            parse_fanout_lane("no-equals"),
            Err(FanoutPlanError::LaneInvalid { .. })
        ));
        assert!(matches!(
            parse_fanout_lane("thread=only-path"),
            Err(FanoutPlanError::LaneInvalid { .. })
        ));
        assert!(matches!(
            parse_fanout_lane("thread=:title"),
            Err(FanoutPlanError::LaneInvalid { .. })
        ));
        assert!(matches!(
            parse_fanout_lane("thread=path:"),
            Err(FanoutPlanError::LaneInvalid { .. })
        ));
    }

    #[test]
    fn parse_lane_rejects_invalid_thread_name() {
        let err = parse_fanout_lane("bad name=./p:Title").unwrap_err();
        assert!(matches!(err, FanoutPlanError::InvalidThreadName { .. }));
    }

    #[test]
    fn parse_lanes_requires_at_least_one() {
        assert_eq!(parse_fanout_lanes(&[]), Err(FanoutPlanError::LaneRequired));
    }

    #[test]
    fn select_parent_thread_attached_and_detached() {
        assert_eq!(select_fanout_parent_thread(Some("main")), "main");
        assert_eq!(select_fanout_parent_thread(Some("  ")), "detached");
        assert_eq!(select_fanout_parent_thread(None), "detached");
    }

    #[test]
    fn select_base_uses_head_facts() {
        let selection = select_fanout_base(&FanoutBaseFacts {
            head_state_full: "abc".into(),
            head_tree_short: "def".into(),
            head_thread: Some("main".into()),
        });
        assert_eq!(selection.base_state, "abc");
        assert_eq!(selection.base_root, "def");
        assert_eq!(selection.parent_thread, "main");

        let detached = select_fanout_base(&FanoutBaseFacts {
            head_state_full: "abc".into(),
            head_tree_short: "def".into(),
            head_thread: None,
        });
        assert_eq!(detached.parent_thread, "detached");
    }

    #[test]
    fn parent_and_child_body_rules() {
        let nodes = vec![
            FanoutNodeSpec {
                thread: "feature/a".into(),
                path: PathBuf::from("../a"),
                title: "Task A".into(),
            },
            FanoutNodeSpec {
                thread: "feature/b".into(),
                path: PathBuf::from("../b"),
                title: "Task B".into(),
            },
        ];
        assert_eq!(
            fanout_parent_body(&nodes),
            "- feature/a: Task A\n- feature/b: Task B"
        );
        assert_eq!(
            fanout_child_body("task-1"),
            "Fan-out child lane for parent task task-1"
        );
        assert_eq!(fanout_parent_delegated_by(), "heddle agent fanout start");
        assert_eq!(fanout_start_attach_rule(), "agent-fanout-start");
    }

    #[test]
    fn plan_fanout_builds_nodes_body_and_start_command() {
        let plan = plan_fanout(&sample_request(&[
            "feature/a=../a:Implement a",
            "feature/b=../b:Implement b",
        ]))
        .unwrap();

        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.parent_thread, "main");
        assert_eq!(plan.base_state, "state-full");
        assert_eq!(plan.base_root, "tree-short");
        assert_eq!(
            plan.coordination_discussion_id.as_deref(),
            Some("discussion-123")
        );
        assert!(plan.parent_body.contains("feature/a: Implement a"));
        assert_eq!(plan.start_commands.len(), 1);
        assert_eq!(plan.start_commands[0].lane_thread, "all");
        let argv = &plan.start_commands[0].argv;
        assert_eq!(argv[0], "heddle");
        assert_eq!(argv[1], "agent");
        assert_eq!(argv[2], "fanout");
        assert_eq!(argv[3], "start");
        assert!(argv.contains(&"--title".to_string()));
        assert!(argv.contains(&"Coordinate fanout".to_string()));
        assert!(argv.contains(&"--coordination-discussion-id".to_string()));
        assert!(argv.contains(&"discussion-123".to_string()));
        assert!(argv.contains(&"feature/a=../a:Implement a".to_string()));
        assert!(argv.contains(&"feature/b=../b:Implement b".to_string()));
        assert!(
            plan.start_commands[0]
                .command
                .contains("agent fanout start")
        );
    }

    #[test]
    fn fanout_start_preflight_blocks_in_priority_order() {
        let ok = FanoutLaneAvailability {
            thread: "a".into(),
            has_live_owner: false,
            thread_ref_exists: false,
            active_thread_record: false,
            resolved_path: PathBuf::from("/tmp/a"),
        };
        assert!(check_fanout_start_preflight(&[ok.clone()]).is_ok());

        let mut live = ok.clone();
        live.has_live_owner = true;
        assert!(matches!(
            check_fanout_start_preflight(&[live]),
            Err(FanoutLanePreflightBlock::LiveOwner { .. })
        ));

        let mut exists = ok.clone();
        exists.thread_ref_exists = true;
        assert!(matches!(
            check_fanout_start_preflight(&[exists]),
            Err(FanoutLanePreflightBlock::ThreadExists { .. })
        ));

        let dup_a = ok.clone();
        let mut dup_b = ok;
        dup_b.thread = "b".into();
        // same resolved path
        assert!(matches!(
            check_fanout_start_preflight(&[dup_a, dup_b]),
            Err(FanoutLanePreflightBlock::DuplicatePath { thread }) if thread == "b"
        ));
    }

    #[test]
    fn plan_fanout_rejects_duplicate_threads() {
        let err = plan_fanout(&sample_request(&[
            "feature/dup=../a:First",
            "feature/dup=../b:Second",
        ]))
        .unwrap_err();
        assert_eq!(
            err,
            FanoutPlanError::DuplicateThread {
                thread: "feature/dup".into()
            }
        );
    }

    #[test]
    fn assemble_plan_report_matches_dry_run_contract() {
        let plan = plan_fanout(&sample_request(&["feature/a=../a:Implement a"])).unwrap();
        let report = assemble_fanout_plan_report(&plan);
        assert_eq!(report.output_kind, "agent_fanout_plan");
        assert!(report.parent_task.is_none());
        assert_eq!(report.lanes.len(), 1);
        assert_eq!(report.lanes[0].status, "planned");
        assert_eq!(report.lanes[0].thread, "feature/a");
        assert_eq!(report.lanes[0].path, "../a");
        assert!(report.lanes[0].task.is_none());
        assert!(report.lanes[0].session_id.is_none());
        assert_eq!(report.commands[0].argv[2], "fanout");
        assert_eq!(report.commands[0].argv[3], "start");

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["output_kind"], "agent_fanout_plan");
        assert!(json["parent_task"].is_null());
        assert_eq!(json["lanes"][0]["status"], "planned");
        assert!(json["lanes"][0]["task"].is_null());
    }
}
