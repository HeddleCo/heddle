// SPDX-License-Identifier: Apache-2.0
//! `heddle stack` command implementation.
//!
//! Top-level surfaces the stack containing the current thread; sub-verbs
//! cover `ready` (next-action verdict) and `snapshot` (JSON projection
//! for agentic tooling). All commands are read-only.

use anyhow::{Result, anyhow};
use refs::Head;
use repo::{
    Repository, RepositorySnapshot, StackNextAction, StackNode, ThreadStack,
};
use serde::Serialize;

use crate::cli::{Cli, StackArgs, StackCommands, should_output_json};

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
        Head::Attached { thread } => Ok(thread),
        Head::Detached { .. } => Err(anyhow!(
            "No current thread (HEAD is detached); pass --thread <name>"
        )),
    }
}

// ── stack (describe) ────────────────────────────────────────────────────

#[derive(Serialize)]
struct StackDescribeOutput {
    thread: String,
    stack: Option<StackEntry>,
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

fn cmd_stack_describe(cli: &Cli, repo: &Repository, thread: Option<String>) -> Result<()> {
    let target = resolve_target_thread(repo, thread)?;
    let stack = repo.thread_stack_for(&target)?;
    let output = StackDescribeOutput {
        thread: target.clone(),
        stack: stack.as_ref().map(stack_to_entry),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    match stack {
        None => {
            println!("Thread {target} is not part of any known stack.");
        }
        Some(stack) => {
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
    thread: String,
    next_action: StackNextAction,
}

fn cmd_stack_ready(cli: &Cli, repo: &Repository, thread: Option<String>) -> Result<()> {
    let target = resolve_target_thread(repo, thread)?;
    let snapshot = RepositorySnapshot::capture(repo)?;
    let action = snapshot.next_action_for(&target)?;

    if should_output_json(cli, Some(repo.config())) {
        let out = StackReadyOutput {
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
    // When `--thread <name>` is given, scope the snapshot to just the
    // stack containing that thread — the documented contract is "snapshot
    // the thread's stack", not "snapshot the whole repo and also check
    // the thread exists". Without scoping, the payload bleeds sibling
    // stacks into per-thread tooling output.
    let snapshot = match thread.as_deref() {
        Some(name) => full.for_stack(name).ok_or_else(|| {
            anyhow!("thread '{name}' is not part of any known stack in this repo")
        })?,
        None => full,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&snapshot)?);
    } else {
        // Text mode still emits JSON — the snapshot is structured data
        // by definition. Pretty-print it so terminal output is readable.
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    }
    Ok(())
}
