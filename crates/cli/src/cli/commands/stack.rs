// SPDX-License-Identifier: Apache-2.0
//! `heddle stack` command implementation.
//!
//! Top-level surfaces the stack containing the current thread; sub-verbs
//! cover `ready` (next-action verdict) and `snapshot` (JSON projection
//! for agentic tooling). All commands are read-only.

use anyhow::{anyhow, Result};
use refs::Head;
use repo::{Repository, RepositorySnapshot, StackNextAction, StackNode, ThreadStack};
use serde::Serialize;

use crate::cli::{should_output_json, Cli, StackArgs, StackCommands};

pub fn cmd_stack(cli: &Cli, args: StackArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let outer_thread = args.thread.clone();

    match args.command {
        None => cmd_stack_describe(cli, &repo, outer_thread),
        Some(StackCommands::Ready { thread }) => {
            cmd_stack_ready(cli, &repo, thread.or(outer_thread))
        }
        Some(StackCommands::Snapshot { thread }) => {
            cmd_stack_snapshot(cli, &repo, thread.or(outer_thread))
        }
    }
}

/// Resolve the thread whose stack we should operate on. Priority:
/// explicit `--thread`, then attached HEAD, then "no current thread".
fn resolve_target_thread(repo: &Repository, explicit: Option<String>) -> Result<String> {
    if let Some(name) = explicit {
        return Ok(name);
    }
    match repo.head_ref()? {
        Head::Attached { thread } => Ok(thread.to_string()),
        Head::Detached { .. } => Err(anyhow!(
            "No current thread (HEAD is detached); pass --thread <name>"
        )),
    }
}

// ── stack (describe) ────────────────────────────────────────────────────

#[derive(Serialize)]
struct StackDescribeOutput {
    output_kind: &'static str,
    /// The thread whose stack we scoped to. `None` when HEAD is detached
    /// and no `--thread` was given — then `stacks` lists every stack in
    /// the repo.
    thread: Option<String>,
    /// Convenience for single-stack output (explicit `--thread` or
    /// attached HEAD). Mirrors `stacks[0]` when present; `None` if the
    /// named thread isn't part of a known stack.
    stack: Option<StackEntry>,
    /// Every stack in the result set. Length 1 for the scoped case,
    /// 0..N for the detached-HEAD list-all case.
    stacks: Vec<StackEntry>,
}

#[derive(Serialize)]
struct StackEntry {
    root: String,
    member_count: usize,
    depth: usize,
    members: Vec<String>,
    tree: TreeNodeJson,
}

#[derive(Serialize)]
struct TreeNodeJson {
    name: String,
    children: Vec<TreeNodeJson>,
}

impl TreeNodeJson {
    fn from_node(node: &StackNode) -> Self {
        Self {
            name: node.name.clone(),
            children: node.children.iter().map(Self::from_node).collect(),
        }
    }
}

fn stack_to_entry(stack: &ThreadStack) -> StackEntry {
    StackEntry {
        root: stack.root_name().to_string(),
        member_count: stack.member_count(),
        depth: stack.depth(),
        members: stack
            .member_names()
            .into_iter()
            .map(str::to_string)
            .collect(),
        tree: TreeNodeJson::from_node(&stack.root),
    }
}

/// Three-way resolution shared by `cmd_stack_describe` and
/// `cmd_stack_snapshot`: explicit `--thread` wins; otherwise an attached
/// HEAD scopes to that thread's stack; a detached HEAD with no
/// `--thread` resolves to `None` (caller should list every stack — the
/// operator has no current thread, so the only useful answer is the
/// full picture).
fn resolve_describe_target(repo: &Repository, explicit: Option<String>) -> Result<Option<String>> {
    Ok(match explicit {
        Some(name) => Some(name),
        None => match repo.head_ref()? {
            Head::Attached { thread } => Some(thread.to_string()),
            Head::Detached { .. } => None,
        },
    })
}

fn cmd_stack_describe(cli: &Cli, repo: &Repository, thread: Option<String>) -> Result<()> {
    let target = resolve_describe_target(repo, thread)?;

    let (stacks, scoped_stack) = match &target {
        Some(name) => {
            let stack = repo.thread_stack_for(name)?;
            let list = stack.iter().cloned().collect::<Vec<_>>();
            (list, stack)
        }
        None => (repo.compute_thread_stacks()?, None),
    };

    let stack_entries: Vec<StackEntry> = stacks.iter().map(stack_to_entry).collect();
    let output = StackDescribeOutput {
        output_kind: "stack",
        thread: target.clone(),
        stack: scoped_stack.as_ref().map(stack_to_entry),
        stacks: stack_entries,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    match (&target, stacks.as_slice()) {
        (Some(name), []) => {
            println!("Thread {name} is not part of any known stack.");
        }
        (_, []) => {
            println!("No stacks in this repo.");
        }
        (_, stacks) => {
            for stack in stacks {
                let depth_label = if stack.member_count() == 1 {
                    "thread".to_string()
                } else {
                    format!("threads, depth {}", stack.depth())
                };
                println!(
                    "{} ({} {depth_label})",
                    stack.root_name(),
                    stack.member_count()
                );
                for child in &stack.root.children {
                    print_tree(child, 1);
                }
            }
        }
    }
    Ok(())
}

fn print_tree(node: &StackNode, depth: usize) {
    // Two spaces per level + an arrow keeps the hierarchy obvious
    // without ASCII-art tree chrome.
    let indent = "  ".repeat(depth - 1);
    println!("{indent}↳ {}", node.name);
    for child in &node.children {
        print_tree(child, depth + 1);
    }
}

// ── stack ready ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StackReadyOutput {
    output_kind: &'static str,
    thread: String,
    next_action: StackNextAction,
}

fn cmd_stack_ready(cli: &Cli, repo: &Repository, thread: Option<String>) -> Result<()> {
    let target = resolve_target_thread(repo, thread)?;
    let snapshot = RepositorySnapshot::capture(repo)?;
    let action = snapshot.next_action_for(&target)?;

    if should_output_json(cli, Some(repo.config())) {
        let out = StackReadyOutput {
            output_kind: "stack_ready",
            thread: target,
            next_action: action,
        };
        println!("{}", serde_json::to_string(&out)?);
        return Ok(());
    }

    match action {
        StackNextAction::Ready => println!("ready"),
        StackNextAction::Blocked { thread: blocking } => {
            println!("blocked by {blocking}");
        }
        StackNextAction::WaitingOnReview { thread: waiting } => {
            println!("waiting-on-review ({waiting})");
        }
        StackNextAction::Unknown => {
            println!("unknown");
        }
    }
    Ok(())
}

// ── stack snapshot ──────────────────────────────────────────────────────

fn cmd_stack_snapshot(cli: &Cli, repo: &Repository, thread: Option<String>) -> Result<()> {
    let full = RepositorySnapshot::capture(repo)?;
    // Scope priority matches the other `heddle stack` subcommands:
    //   1. explicit `--thread <name>` → scope to that thread's stack.
    //   2. attached HEAD → scope to the HEAD thread's stack (same
    //      default operators get from `heddle stack` / `stack ready`).
    //   3. detached HEAD → emit the full repo snapshot (every stack);
    //      there's no current thread to scope to, so the only useful
    //      answer is the full picture, matching `heddle stack`'s
    //      detached behavior.
    // Without (2), the snapshot bled sibling stacks into per-thread
    // tooling output whenever `--thread` was omitted from an attached
    // checkout.
    let target_thread = resolve_describe_target(repo, thread)?;
    let snapshot = match target_thread.as_deref() {
        Some(name) => full.for_stack(name).ok_or_else(|| {
            anyhow!("thread '{name}' is not part of any known stack in this repo")
        })?,
        None => full,
    };

    let envelope = StackSnapshotOutput {
        output_kind: "stack_snapshot",
        snapshot: &snapshot,
    };
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&envelope)?);
    } else {
        // Text mode still emits JSON — the snapshot is structured data
        // by definition. Pretty-print it so terminal output is readable.
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    }
    Ok(())
}

#[derive(Serialize)]
struct StackSnapshotOutput<'a> {
    output_kind: &'static str,
    #[serde(flatten)]
    snapshot: &'a RepositorySnapshot,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use objects::object::ChangeId;
    use refs::Head;
    use repo::{Repository, ThreadFreshness, ThreadManager, ThreadMode, ThreadRecord, ThreadState};

    use super::*;

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    fn save_record(manager: &ThreadManager, name: &str, parent: Option<&str>) {
        let record = ThreadRecord {
            id: format!("rec-{name}"),
            thread: name.to_string(),
            target_thread: parent.map(str::to_string),
            parent_thread: parent.map(str::to_string),
            mode: ThreadMode::Materialized,
            state: ThreadState::Active,
            base_state: "main-1".to_string(),
            base_root: "main-1".to_string(),
            current_state: Some(format!("{name}-tip")),
            merged_state: None,
            task: None,
            changed_paths: Vec::new(),
            impact_categories: Vec::new(),
            heavy_impact_paths: Vec::new(),
            promotion_suggested: false,
            freshness: ThreadFreshness::Current,
            verification_summary: Default::default(),
            confidence_summary: Default::default(),
            integration_policy_result: Default::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            ephemeral: None,
            auto: false,
            shared_target_dir: None,
        };
        manager.save_record(&record).unwrap();
    }

    #[test]
    fn resolve_describe_target_prefers_explicit_thread_over_head() {
        let (_temp, repo) = init_repo();
        repo.refs()
            .write_head(&Head::Attached {
                thread: "head-thread".into(),
            })
            .unwrap();
        let target = resolve_describe_target(&repo, Some("explicit".into())).unwrap();
        assert_eq!(target, Some("explicit".to_string()));
    }

    #[test]
    fn resolve_describe_target_defaults_to_attached_head_thread() {
        let (_temp, repo) = init_repo();
        repo.refs()
            .write_head(&Head::Attached {
                thread: "feat-a".into(),
            })
            .unwrap();
        let target = resolve_describe_target(&repo, None).unwrap();
        assert_eq!(target, Some("feat-a".to_string()));
    }

    #[test]
    fn resolve_describe_target_yields_none_when_head_is_detached() {
        // The bug fix: previously `heddle stack` (and `stack snapshot`)
        // hard-errored on detached HEAD via `resolve_target_thread`.
        // The new contract is "no current thread → list every stack",
        // signalled by `Ok(None)` so the caller can switch to the
        // describe-all branch.
        let (_temp, repo) = init_repo();
        let id = ChangeId::generate();
        repo.refs()
            .write_head(&Head::Detached { state: id })
            .unwrap();
        let target = resolve_describe_target(&repo, None).unwrap();
        assert_eq!(target, None);
    }

    #[test]
    fn cmd_stack_describe_lists_every_stack_when_head_is_detached() {
        // End-to-end: two disjoint stacks in the repo + detached HEAD.
        // `cmd_stack_describe` must succeed (not error) and the describe
        // path must walk *all* stacks. We can't easily capture stdout
        // here, so we re-derive the same set the handler uses and pin
        // the count + roots — if the handler regressed to "scope to
        // HEAD only", `compute_thread_stacks` wouldn't be reached at
        // all and `cmd_stack_describe` would have errored before
        // printing.
        let (_temp, repo) = init_repo();
        let manager = ThreadManager::new(repo.heddle_dir());
        save_record(&manager, "feat-a", None);
        save_record(&manager, "feat-b", Some("feat-a"));
        save_record(&manager, "infra-x", None);

        let id = ChangeId::generate();
        repo.refs()
            .write_head(&Head::Detached { state: id })
            .unwrap();

        let cli = Cli {
            command: crate::cli::Commands::Stack(StackArgs {
                thread: None,
                command: None,
            }),
            output: Some(crate::cli::OutputMode::Json),
            no_color: true,
            repo: Some(repo.root().to_path_buf()),
            verbose: 0,
            quiet: false,
            op_id: None,
        };
        cmd_stack_describe(&cli, &repo, None).expect("detached HEAD must not error");

        // And confirm the underlying surface really does see both stacks
        // — that's what the handler now walks.
        let stacks = repo.compute_thread_stacks().unwrap();
        assert_eq!(stacks.len(), 2);
        let roots: Vec<&str> = stacks.iter().map(|s| s.root_name()).collect();
        assert!(roots.contains(&"feat-a") && roots.contains(&"infra-x"));
    }
}
