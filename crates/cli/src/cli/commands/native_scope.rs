// SPDX-License-Identifier: Apache-2.0
//! Honest boundary for the native-only annotation surfaces (`heddle context`
//! and `heddle discuss`).
//!
//! Context annotations and discussions are a *native* Heddle feature. They
//! live in `.heddle/`, they travel over `heddle push` / `heddle pull` to a
//! Heddle remote, and they are deliberately **not** projected into Git — not
//! into `refs/notes/*`, not into a tracked file (heddle#1145). A `git clone`
//! therefore carries none of them.
//!
//! Two things used to hide that boundary, and both were silent:
//!
//! 1. Read-only commands called [`Repository::open`], which bootstraps a
//!    Git-overlay sidecar when it finds a plain Git tree. Running
//!    `heddle context list` in a fresh clone *created* a `.heddle` store as a
//!    side effect, so the following `heddle status` reported
//!    `Repository: Git + Heddle`. The clone had never had a store.
//! 2. Having created that empty store, the same command reported
//!    "No context annotations." — the identical wording a genuinely empty
//!    native repository produces. "Your annotations did not come with the
//!    clone" and "this repository has no annotations" were indistinguishable.
//!
//! This module supplies the two halves of the fix: [`open_annotation_store`]
//! resolves the surface *without* bootstrapping, so an absent store stays
//! absent and is reported as absent; and [`emit_locality_notice_once`] tells
//! the user, once per working copy, that what they just wrote is local to it.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use repo::{Repository, RepositoryCapability, discover_heddle_root};
use sley::Repository as SleyRepository;

use super::action_line::print_command;
use crate::cli::{Cli, should_output_json, style};

/// Which annotation surface is asking. Drives wording only; both surfaces
/// share the same storage boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnnotationSurface {
    Context,
    Discuss,
}

impl AnnotationSurface {
    /// Plural noun for the records this surface manages.
    fn records(self) -> &'static str {
        match self {
            Self::Context => "context annotations",
            Self::Discuss => "discussions",
        }
    }

    /// Key holding the (always empty) result set, so an absent-store envelope
    /// stays shape-compatible with the populated one. Only the list surfaces
    /// carry a collection; the rest report absence without one.
    fn items_key(self) -> &'static str {
        match self {
            Self::Context => "items",
            Self::Discuss => "discussions",
        }
    }

    /// Basename of the per-working-copy marker recording that the locality
    /// notice has already been shown for this surface.
    fn notice_marker(self) -> &'static str {
        match self {
            Self::Context => "context-locality-notice",
            Self::Discuss => "discuss-locality-notice",
        }
    }

    fn first_step(self) -> &'static str {
        match self {
            Self::Context => "heddle context set --path <path> --scope file -m \"...\"",
            Self::Discuss => "heddle discuss open <file> <symbol> '<question>'",
        }
    }
}

/// The resolved annotation surface for a command.
pub(crate) enum AnnotationStore {
    /// A Heddle store backs this path; read and write it normally.
    Present(Box<Repository>),
    /// No Heddle store here. Nothing was created to make that true.
    Absent(AbsentStore),
}

/// Why there is no store, which is what makes the report actionable: a Git
/// checkout can gain a local one with `heddle init`, a bare directory needs a
/// repository first.
pub(crate) struct AbsentStore {
    path: PathBuf,
    git_root: Option<PathBuf>,
}

impl AbsentStore {
    /// Whether the path is a Git worktree that simply has no `.heddle`. This
    /// is the `git clone` case from heddle#1145.
    fn in_git_checkout(&self) -> bool {
        self.git_root.is_some()
    }
}

/// Resolve the path a command should act on: `--repo` when given, else the
/// current directory. Mirrors [`Cli::open_repo`] so the classification below
/// describes the same path the command would have opened.
fn command_path(cli: &Cli) -> Result<PathBuf> {
    match cli.repo.as_ref() {
        Some(path) => Ok(path.clone()),
        None => std::env::current_dir().context("get current working directory"),
    }
}

/// Open the Heddle store backing `cli`'s path **without bootstrapping one**.
///
/// [`Repository::open`] deliberately bootstraps a Git-overlay sidecar for
/// mutating commands that need one (`import`, `start`, `marker`, …). Read-only
/// annotation commands must not: creating a store in order to report it empty
/// is what made `heddle status` claim `Repository: Git + Heddle` in a clone
/// that had no store at all.
///
/// Discovery reuses [`discover_heddle_root`] (heddle#1147/#1180), which
/// requires a repository-specific member under `.heddle` rather than accepting
/// the directory name — so the user configuration directory does not read as a
/// repository.
pub(crate) fn open_annotation_store(cli: &Cli) -> Result<AnnotationStore> {
    let path = command_path(cli)?;
    if discover_heddle_root(&path).is_some() {
        return Ok(AnnotationStore::Present(Box::new(cli.open_repo()?)));
    }
    Ok(AnnotationStore::Absent(AbsentStore {
        git_root: plain_git_root(&path),
        path,
    }))
}

/// Locate the Git worktree containing `path`, if any. Probe only — it opens
/// nothing Heddle-side and creates nothing.
fn plain_git_root(path: &Path) -> Option<PathBuf> {
    SleyRepository::open_from_environment(path)
        .ok()
        .and_then(|git| git.workdir())
}

/// Report that no Heddle store backs this path — distinct from an empty one.
///
/// Exits successfully: the command answered the question it was asked. The
/// answer is "there is no store here", not "there are no records", and the
/// text and the machine envelope both say so.
///
/// `output_kind` is the caller's own discriminator so the absent-store
/// envelope stays a valid instance of the shape that command always emits.
/// `with_items` adds the surface's empty collection, for the `list` commands
/// whose populated envelope carries one.
pub(crate) fn report_absent_store(
    cli: &Cli,
    surface: AnnotationSurface,
    output_kind: &str,
    with_items: bool,
    absent: &AbsentStore,
) -> Result<()> {
    let records = surface.records();
    if should_output_json(cli, None) {
        let mut envelope = serde_json::json!({
            "output_kind": output_kind,
            "store_present": false,
            "store_scope": "native-heddle-only",
            "path": absent.path.display().to_string(),
            "git_checkout": absent.in_git_checkout(),
            "reason": format!(
                "no Heddle store at this path; {records} are native-only and are not carried by git clone"
            ),
        });
        if with_items && let Some(map) = envelope.as_object_mut() {
            map.insert(surface.items_key().to_string(), serde_json::json!([]));
        }
        println!("{}", serde_json::to_string(&envelope)?);
        return Ok(());
    }

    println!("No Heddle store here — cannot say whether {records} exist.");
    if absent.in_git_checkout() {
        println!(
            "This is a Git checkout with no `.heddle` store, and {records} are a native Heddle \
             feature — not projected into Git, so `git clone` does not carry them."
        );
        println!();
        println!("{}", style::bold("Next"));
        print_command("heddle init");
        println!(
            "  then: {}   (records stay local to this working copy)",
            surface.first_step()
        );
    } else {
        println!(
            "No Heddle repository was found at {} or in its ancestors.",
            absent.path.display()
        );
        println!();
        println!("{}", style::bold("Next"));
        print_command("heddle init");
    }
    Ok(())
}

/// Tell the user, **once per working copy**, that what they just created does
/// not travel with Git.
///
/// Only fires in Git Overlay mode: in a native repository the records do
/// travel (over `heddle push`/`heddle pull`), so the notice would be false.
/// The marker lives under `.heddle/`, which is excluded from Git, so each
/// clone and each working copy gets the notice exactly once — it is a
/// boundary statement at creation time, not a recurring nag.
///
/// Written to stderr: it is human advice, not part of the machine contract,
/// and stderr keeps `--output json` stdout parseable.
pub(crate) fn emit_locality_notice_once(repo: &Repository, surface: AnnotationSurface) {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return;
    }
    let Some(marker) = notice_marker_path(repo, surface) else {
        return;
    };
    if marker.exists() {
        return;
    }
    // Best-effort: a marker we cannot write means the notice may repeat, which
    // is far better than failing the annotation the user asked us to create.
    if let Some(parent) = marker.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return;
    }
    if fs::write(&marker, b"shown\n").is_err() {
        return;
    }
    eprintln!(
        "Note: {} are local to this working copy — they live in `.heddle` and do not travel \
         with `git push` / `git clone`. See `heddle help {}`.",
        surface.records(),
        match surface {
            AnnotationSurface::Context => "context",
            AnnotationSurface::Discuss => "discuss",
        }
    );
}

/// Per-working-copy marker path. Anchored at the checkout root rather than
/// [`Repository::heddle_dir`], which for a linked worktree points at the
/// *shared* store — the notice is about this working copy.
fn notice_marker_path(repo: &Repository, surface: AnnotationSurface) -> Option<PathBuf> {
    let local = repo.root().join(".heddle");
    let base = if local.is_dir() {
        local
    } else {
        repo.heddle_dir().to_path_buf()
    };
    Some(base.join("state").join(surface.notice_marker()))
}
