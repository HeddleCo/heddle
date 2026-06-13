// SPDX-License-Identifier: Apache-2.0
use objects::store::ObjectStore;
use std::path::{Path, PathBuf};

use anyhow::Result;
use objects::object::ChangeId;
use repo::{AudienceTier, CheckoutMaterialization, Repository};

use super::super::advice::RecoveryAdvice;

/// The prepared `--path` target plus whether THIS invocation created the
/// target directory. A compensating rollback must only undo what this
/// invocation created: a directory we created is removed entirely, but a
/// pre-existing empty directory the user supplied is preserved (only the
/// contents we materialized inside it are cleared) — never destroy user
/// state we merely wrote into.
pub(crate) struct PreparedWorktreeTarget {
    pub path: PathBuf,
    pub target_dir_created: bool,
}

pub(crate) fn prepare_worktree_target(
    repo: &Repository,
    path: &Path,
    self_thread: Option<&str>,
) -> Result<PreparedWorktreeTarget> {
    let prepared = plan_worktree_target(repo, path, self_thread)?;
    let requested = absolute_path(path)?;
    std::fs::create_dir_all(&prepared.path).map_err(|error| {
        anyhow::anyhow!(worktree_target_prepare_failed_advice(&requested, error))
    })?;
    validate_worktree_target(repo, &prepared.path, self_thread)?;
    Ok(prepared)
}

/// Validate + resolve a `--path` target WITHOUT creating the directory, and
/// report whether the resolved target is absent (so the caller can create it
/// itself and know to remove it on rollback).
///
/// The atomic `thread start` path uses this so the target-dir creation happens
/// *inside* the transaction (its first step), not before `execute` has a rewind
/// ledger — otherwise a failure in the remaining pre-transaction work would
/// orphan a directory this command created (heddle#356 cid 3333881552).
pub(crate) fn plan_worktree_target(
    repo: &Repository,
    path: &Path,
    self_thread: Option<&str>,
) -> Result<PreparedWorktreeTarget> {
    let requested = absolute_path(path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&requested)
        && metadata.file_type().is_symlink()
    {
        return Err(anyhow::anyhow!(worktree_target_symlink_advice(&requested)));
    }
    let resolved = canonicalize_existing_ancestor(&requested)?;
    validate_worktree_target(repo, &resolved, self_thread)?;
    // Capture pre-existence: this is the only point where "the user gave us an
    // existing empty dir" vs "we will create it" is still distinguishable. The
    // creation itself is deferred to the caller (the transaction) so a failure
    // before the dir is made leaves nothing to clean up.
    let target_dir_created = !resolved.exists();
    Ok(PreparedWorktreeTarget {
        path: resolved,
        target_dir_created,
    })
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn canonicalize_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut ancestor = path;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| anyhow::anyhow!(worktree_target_invalid_path_advice(path)))?;
    }

    let mut resolved = ancestor.canonicalize()?;
    let remainder = path
        .strip_prefix(ancestor)
        .map_err(|_| anyhow::anyhow!(worktree_target_invalid_path_advice(path)))?;

    for component in remainder.components() {
        match component {
            std::path::Component::Normal(part) => resolved.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::Prefix(_)
            | std::path::Component::RootDir => {
                return Err(anyhow::anyhow!(worktree_target_unsafe_path_advice(path)));
            }
        }
    }

    Ok(resolved)
}

fn validate_worktree_target(
    repo: &Repository,
    path: &Path,
    self_thread: Option<&str>,
) -> Result<()> {
    // `.heddle/threads/` is the managed home for thread checkouts (the
    // default for `heddle start`), so it's explicitly allowed even though
    // it sits under `.heddle/`. Everything else under `.heddle/` is repo
    // metadata storage and stays off-limits — a checkout there could
    // corrupt the store/refs/oplog.
    let threads_root = repo.heddle_dir().join("threads");
    let in_threads_root = path == threads_root || path.starts_with(&threads_root);
    if in_threads_root {
        // The threads metadata root ITSELF is off-limits: checking out at
        // `.heddle/threads` would write the worktree's `.heddle/` and the
        // per-thread `manifest.toml` on top of the thread-metadata root,
        // breaking the layout invariant (heddle#572 r2). A descendant
        // per-thread slot is what's allowed, not the root.
        if path == threads_root {
            return Err(anyhow::anyhow!(worktree_target_nested_thread_advice(path)));
        }
        // A managed checkout must be a per-thread *leaf* (`threads/<seg>/root`),
        // never the bare per-thread directory `threads/<seg>` itself. The
        // per-thread `manifest.toml` sidecar lives AT `threads/<encoded>/`
        // (a sibling of `root/`); a checkout placed on the per-thread dir
        // itself would make the sidecar a CHILD of the worktree — git keeps
        // its worktree metadata at the worktree root, outside tracked content,
        // and so must we. So `start foo --path .heddle/threads/foo` (no `root`
        // leaf) would write `manifest.toml` inside foo's own checkout, starting
        // it dirty and able to capture the sidecar as user content. Require the
        // leaf so the sidecar stays OUTSIDE the checkout (heddle#572 r3).
        if path.parent() == Some(threads_root.as_path()) {
            return Err(anyhow::anyhow!(worktree_target_managed_needs_leaf_advice(
                path
            )));
        }
        // Under `.heddle/threads` is allowed for managed checkouts, but the
        // target must be a fresh per-thread slot — never nested inside an
        // EXISTING thread's reserved subtree (its canonical
        // `.heddle/threads/<name>/` dir OR its durable recorded checkout/mount
        // path). `is_inside_existing_thread` enumerates EVERY existing thread
        // (solid, materialized, AND virtualized) via its record + the shared
        // `thread_dir` derivation, so an explicit `--path` nested inside
        // another thread's checkout is rejected for any workspace mode. The
        // thread's OWN reserved root is exempt only for `self_thread` (a
        // promote/re-materialize of that same thread), so one thread can never
        // reuse another's reserved root (heddle#572 r2/r3).
        if is_inside_existing_thread(repo, &threads_root, path, self_thread)? {
            return Err(anyhow::anyhow!(worktree_target_nested_thread_advice(path)));
        }
    } else if path == repo.heddle_dir() || path.starts_with(repo.heddle_dir()) {
        return Err(anyhow::anyhow!(worktree_target_storage_advice(path)));
    }

    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(anyhow::anyhow!(worktree_target_symlink_advice(path)));
        }

        if !metadata.is_dir() {
            return Err(anyhow::anyhow!(worktree_target_not_directory_advice(path)));
        }

        if std::fs::read_dir(path)?.next().transpose()?.is_some() {
            return Err(anyhow::anyhow!(worktree_target_not_empty_advice(path)));
        }
    }

    Ok(())
}

/// True if `candidate` (already known to live under `threads_root`) falls
/// inside an EXISTING thread's reserved space — for ANY workspace mode.
///
/// Existing threads are enumerated from the durable thread RECORDS (not the
/// filesystem). For each we check two reserved regions:
///   * the canonical per-thread directory `.heddle/threads/<encoded>/`,
///     recomputed with the shared [`repo::thread_manifest::thread_dir`]
///     derivation (the mode-agnostic source of truth: `solid`/`virtualized`
///     starts write a record but no `manifest.toml`, so a manifest-only check
///     silently missed them — heddle#572 r1/r2);
///   * the thread's DURABLE recorded `execution_path` / `materialized_path`
///     (filtered to ones under `threads_root`). A thread started with a custom
///     `--path .heddle/threads/<custom>/root` checks out OUTSIDE its canonical
///     `thread_dir`, so a canonical-only check would let a later `--path` nest
///     under that custom checkout. Consulting the recorded paths closes that
///     gap (heddle#572 r3).
///
/// The record is written strictly AFTER target validation for a *new* thread
/// (the atomic `start` stages it inside `execute`; the harness `save`s it after
/// `prepare_worktree_target`), so a brand-new thread is never in this set — its
/// fresh `.heddle/threads/<new>/root` slot is correctly allowed.
///
/// A thread's OWN reserved root (canonical `<encoded>/root`, or its recorded
/// checkout path) is exempt ONLY for `self_thread` — the same thread being
/// promoted/re-materialized, whose record is already present at validation
/// time. For every OTHER caller the exemption does NOT apply, so a fresh
/// `start other --path .heddle/threads/<existing>/root` can never reuse another
/// thread's reserved root (heddle#572 r3 Finding #2).
fn is_inside_existing_thread(
    repo: &Repository,
    threads_root: &Path,
    candidate: &Path,
    self_thread: Option<&str>,
) -> Result<bool> {
    // Cheap guard: only paths under `threads_root` can be inside a thread dir.
    if !candidate.starts_with(threads_root) {
        return Ok(false);
    }
    for thread in repo::ThreadManager::new(repo.heddle_dir()).list()? {
        let is_self = self_thread == Some(thread.thread.as_str());

        // Canonical per-thread directory.
        let dir = repo::thread_manifest::thread_dir(repo.heddle_dir(), &thread.thread);
        if candidate.starts_with(&dir) && !(is_self && candidate == dir.join("root")) {
            return Ok(true);
        }

        // Durable recorded checkout/mount paths under `threads_root`. A path
        // outside `threads_root` (a sibling-dir checkout, or the repo root for
        // a non-isolated thread) can never be an ancestor of a `threads_root`
        // candidate, so filtering both avoids false positives.
        for recorded in [
            Some(&thread.execution_path),
            thread.materialized_path.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if recorded.as_os_str().is_empty() || !recorded.starts_with(threads_root) {
                continue;
            }
            if candidate.starts_with(recorded) && !(is_self && candidate == recorded.as_path()) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn worktree_target_symlink_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_symlink",
        format!("worktree target '{}' cannot be a symlink", path.display()),
        "Choose an empty real directory for `--path`, or let Heddle create a managed materialized checkout.",
        format!(
            "target path '{}' resolves through a symlink",
            path.display()
        ),
        "writing an isolated checkout through a symlink could target a different location than the caller sees",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --workspace materialized",
        vec![
            "heddle start <name> --workspace materialized".to_string(),
            "heddle start <name> --path <empty-path>".to_string(),
        ],
    )
}

fn worktree_target_prepare_failed_advice(path: &Path, error: std::io::Error) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_prepare_failed",
        format!(
            "Could not prepare isolated thread workspace '{}': {error}",
            path.display()
        ),
        "Choose an empty writable path with `--path`, or let Heddle create a managed materialized checkout.",
        format!("target path '{}' could not be created", path.display()),
        "continuing would leave the isolated checkout only partially prepared",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --workspace materialized",
        vec![
            "heddle start <name> --workspace materialized".to_string(),
            "heddle start <name> --path <empty-path>".to_string(),
        ],
    )
}

fn worktree_target_invalid_path_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_invalid_path",
        format!("invalid worktree path '{}'", path.display()),
        "Choose an empty writable path for `--path`, or let Heddle create a managed materialized checkout.",
        format!("target path '{}' has no usable ancestor", path.display()),
        "continuing would make checkout placement ambiguous",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --workspace materialized",
        vec![
            "heddle start <name> --workspace materialized".to_string(),
            "heddle start <name> --path <empty-path>".to_string(),
        ],
    )
}

fn worktree_target_unsafe_path_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_unsafe_path",
        format!("unsafe worktree path '{}'", path.display()),
        "Choose a normal empty path for `--path`; avoid parent-directory traversal.",
        format!(
            "target path '{}' contains an unsafe component",
            path.display()
        ),
        "continuing could write outside the intended checkout location",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --path <empty-path>",
        vec!["heddle start <name> --path <empty-path>".to_string()],
    )
}

fn worktree_target_storage_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_in_heddle_storage",
        format!(
            "worktree target '{}' cannot point into .heddle storage",
            path.display()
        ),
        "Choose a checkout path outside `.heddle`, preferably a sibling directory.",
        format!(
            "target path '{}' is inside repository metadata storage",
            path.display()
        ),
        "writing a checkout there could corrupt Heddle repository metadata",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --path ../<name>",
        vec!["heddle start <name> --path ../<name>".to_string()],
    )
}

fn worktree_target_nested_thread_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_nested_thread",
        format!(
            "worktree target '{}' is nested inside an existing thread's checkout",
            path.display()
        ),
        "Choose a fresh `.heddle/threads/<name>` slot or a sibling directory outside the repository.",
        format!(
            "target path '{}' falls under another thread's reserved `.heddle/threads/<name>` subtree",
            path.display()
        ),
        "writing a checkout there would land it inside another thread's worktree",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --workspace materialized",
        vec![
            "heddle start <name> --workspace materialized".to_string(),
            "heddle start <name> --path ../<name>".to_string(),
        ],
    )
}

fn worktree_target_managed_needs_leaf_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_managed_needs_leaf",
        format!(
            "managed worktree target '{}' must be a per-thread checkout leaf, not the per-thread directory itself",
            path.display()
        ),
        "Append a `root` leaf, e.g. `<path>/root`, or let Heddle pick the default `.heddle/threads/<name>/root`.",
        format!(
            "target path '{}' is the bare `.heddle/threads/<name>` directory, where the per-thread manifest sidecar lives",
            path.display()
        ),
        "checking out onto the per-thread directory would write Heddle's `manifest.toml` sidecar inside the worktree, starting it dirty",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --workspace materialized",
        vec![
            "heddle start <name> --workspace materialized".to_string(),
            "heddle start <name> --path <path>/root".to_string(),
        ],
    )
}

fn worktree_target_not_directory_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_not_directory",
        format!("worktree target '{}' must be a directory", path.display()),
        "Choose an empty directory path for `--path`, or let Heddle create a managed materialized checkout.",
        format!(
            "target path '{}' exists but is not a directory",
            path.display()
        ),
        "continuing would overwrite a non-directory path",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --workspace materialized",
        vec![
            "heddle start <name> --workspace materialized".to_string(),
            "heddle start <name> --path <empty-path>".to_string(),
        ],
    )
}

fn worktree_target_not_empty_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_not_empty",
        format!("worktree target '{}' is not empty", path.display()),
        "Use an empty path, capture current work with `heddle capture`, or let Heddle create a managed materialized checkout.",
        format!("target path '{}' already contains files", path.display()),
        "writing an isolated checkout there could overwrite or mix with existing work",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --workspace materialized",
        vec![
            "heddle start <name> --workspace materialized".to_string(),
            "heddle start <name> --path <empty-path>".to_string(),
            "heddle capture -m \"...\"".to_string(),
        ],
    )
}

pub(crate) fn write_isolated_checkout(
    repo: &Repository,
    abs_path: &Path,
    base_state: &ChangeId,
    thread: Option<&str>,
) -> Result<CheckoutMaterialization> {
    let heddle_dir = abs_path.join(".heddle");
    if heddle_dir.exists() {
        return Err(anyhow::anyhow!(worktree_target_existing_heddle_advice(
            abs_path
        )));
    }
    let shared_galeed_dir = repo.heddle_dir();
    std::fs::create_dir_all(&heddle_dir)?;
    {
        use std::io::Write as _;
        let mut pointer_file = std::fs::File::create(heddle_dir.join("objectstore"))?;
        pointer_file
            .write_all(format!("objectstore: {}\n", shared_galeed_dir.display()).as_bytes())?;
        pointer_file.sync_all()?;
    }
    std::fs::create_dir_all(heddle_dir.join("state"))?;
    // Fault point for the partial-materialize rollback test (heddle#356):
    // the checkout's `.heddle` metadata is already on disk here but no tree
    // bytes are, modeling a materialize that fails partway. The transaction's
    // checkout-rewind inverse must remove the whole created tree (incl
    // `.heddle`) — or, for a user-supplied pre-existing dir, clear its
    // contents. No-op in production (env var unset).
    objects::fault_inject::maybe_fail_at("start_materialize_checkout")
        .map_err(|error| anyhow::anyhow!(error))?;

    let checkout_head = heddle_dir.join("HEAD");
    let head_content = match thread {
        Some(thread) => format!("ref: {}\n", thread),
        None => format!("{}\n", base_state.to_string_full()),
    };
    {
        use std::io::Write as _;
        let mut head_file = std::fs::File::create(&checkout_head)?;
        head_file.write_all(head_content.as_bytes())?;
        head_file.sync_all()?;
    }

    let state = repo
        .store()
        .get_state(base_state)?
        .ok_or_else(|| anyhow::anyhow!("State not found in object store"))?;
    // Route through the visibility-gated checkout chokepoint rather than calling
    // the raw `materialize_tree`. `heddle start --path` reaches the materializer
    // HERE, not through `materialize_thread`, so the gate must live at this
    // chokepoint too or an embargoed state's bytes leak into the checkout
    // (#316 / PR #528 Finding 2). Operator-local checkouts use the all-seeing
    // `Internal` audience; a `Private` state is withheld even from `Internal`
    // (fail closed) and the checkout receives the courtesy stub instead.
    //
    // PROPAGATE the gate outcome to the caller (do NOT discard it): when the base
    // state is withheld, only the courtesy stub is on disk — the real tree was
    // intentionally not materialized. The atomic start path uses this to record a
    // WITHHELD-consistent manifest instead of stat-ing the unmaterialized real
    // tree, so `heddle start` on a Private base yields a withheld checkout rather
    // than erroring (#316 / PR #528 r9 Finding 3).
    let outcome =
        repo.checkout_state_gated(base_state, &state, abs_path, &AudienceTier::Internal)?;
    Ok(outcome)
}

fn worktree_target_existing_heddle_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_existing_heddle",
        format!("'{}' already has a .heddle directory", path.display()),
        "Choose a path that is not already a Heddle checkout.",
        format!(
            "target path '{}' already contains Heddle checkout metadata",
            path.display()
        ),
        "reusing that path could attach the new thread to the wrong checkout metadata",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --path <empty-path>",
        vec!["heddle start <name> --path <empty-path>".to_string()],
    )
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use chrono::Utc;
    use objects::object::{Principal, StateVisibility, ThreadName, VisibilityTier};
    use tempfile::TempDir;

    // The operator-local courtesy placeholder filename written by the gated
    // checkout chokepoint when a state is under-tier for the audience. Mirrored
    // here (the const itself is repo-crate-private) to assert the leak is
    // closed at this entry point too.
    const COURTESY_STUB_FILENAME: &str = "HEDDLE-EMBARGO.txt";

    fn embargo_head(repo: &Repository) -> ChangeId {
        let state_id = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("head present");
        repo.put_state_visibility(StateVisibility {
            state: state_id,
            tier: VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
            embargo_until: None,
            declarer: Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            },
            declared_at: Utc::now(),
            signature: None,
            supersedes: None,
        })
        .expect("put visibility");
        state_id
    }

    /// #316 / PR #528 Finding 2: `heddle start --path` reaches the materializer
    /// via `write_isolated_checkout`, not `materialize_thread`. The visibility
    /// gate must cover this chokepoint too, or an embargoed state's bytes leak
    /// into the checkout. An under-tier state gets the courtesy stub, never its
    /// tracked content.
    #[test]
    fn write_isolated_checkout_withholds_embargoed_state() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        std::fs::write(repo_dir.path().join("secret.rs"), b"fn exploit() {}\n").unwrap();
        repo.snapshot(Some("embargoed".into()), None).unwrap();
        let state_id = embargo_head(&repo);

        let holder = TempDir::new().unwrap();
        let dest = holder.path().join("out");
        write_isolated_checkout(&repo, &dest, &state_id, Some("main")).expect("checkout");

        assert!(
            dest.join(COURTESY_STUB_FILENAME).exists(),
            "embargoed start --path must write the courtesy stub"
        );
        assert!(
            !dest.join("secret.rs").exists(),
            "embargoed bytes must NOT be materialized via write_isolated_checkout"
        );
    }

    /// Register a thread record (any mode) the way a completed `start`
    /// would, so the registry-based nesting guard can see it. Materialized
    /// threads also write a `manifest.toml`; solid and virtualized threads do
    /// NOT — the guard must recognise them all from the record alone.
    fn register_thread(repo: &Repository, name: &str, mode: repo::ThreadMode) {
        register_thread_at(repo, name, mode, PathBuf::new());
    }

    /// Like [`register_thread`], but with an explicit durable
    /// `execution_path`/`materialized_path` — used to exercise the
    /// recorded-path branch of the nesting guard for threads checked out at a
    /// CUSTOM `--path` (outside their canonical `thread_dir`).
    fn register_thread_at(
        repo: &Repository,
        name: &str,
        mode: repo::ThreadMode,
        execution_path: PathBuf,
    ) {
        register_thread_inner(repo, name, mode, execution_path)
    }

    fn register_thread_inner(
        repo: &Repository,
        name: &str,
        mode: repo::ThreadMode,
        execution_path: PathBuf,
    ) {
        let now = Utc::now();
        let thread = repo::Thread {
            id: name.to_string(),
            thread: name.to_string(),
            target_thread: None,
            parent_thread: None,
            mode,
            state: repo::ThreadState::Active,
            base_state: "deadbeef".to_string(),
            base_root: "deadbeef".to_string(),
            current_state: None,
            merged_state: None,
            task: None,
            materialized_path: (!execution_path.as_os_str().is_empty())
                .then(|| execution_path.clone()),
            execution_path,
            changed_paths: Vec::new(),
            impact_categories: Vec::new(),
            heavy_impact_paths: Vec::new(),
            promotion_suggested: false,
            freshness: repo::ThreadFreshness::Unknown,
            verification_summary: Default::default(),
            confidence_summary: Default::default(),
            integration_policy_result: Default::default(),
            created_at: now,
            updated_at: now,
            ephemeral: None,
            auto: false,
            shared_target_dir: None,
        };
        repo::ThreadManager::new(repo.heddle_dir())
            .save(&thread)
            .expect("save thread record");
    }

    /// An explicit `--path` may live under `.heddle/threads/<newname>` (the
    /// managed home for checkouts) but must NOT nest inside an EXISTING
    /// thread's reserved subtree — that would land the new checkout inside
    /// another thread's `root/` worktree or mount. The guard enumerates
    /// threads from their records, so it covers EVERY workspace mode —
    /// including `virtualized`, which writes no `manifest.toml` (heddle#572
    /// r2). A fresh leaf, the threads-root, the bare per-thread dir, custom
    /// recorded checkouts, and the same-thread promote exemption are exercised.
    #[test]
    fn validate_rejects_path_nested_in_existing_thread_checkout() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        let threads_root = repo.heddle_dir().join("threads");

        // An existing MATERIALIZED thread `foo` and an existing VIRTUALIZED
        // thread `virt`. Both occupy `.heddle/threads/<name>/` with a `root/`
        // worktree/mount; only `foo` has a `manifest.toml`.
        register_thread(&repo, "foo", repo::ThreadMode::Materialized);
        register_thread(&repo, "virt", repo::ThreadMode::Virtualized);
        for name in ["foo", "virt"] {
            let nested = threads_root.join(name).join("root").join("nested");
            let err = validate_worktree_target(&repo, &nested, None).unwrap_err();
            assert!(
                err.to_string().contains("nested inside an existing thread"),
                "path nested in existing {name} thread must be rejected: {err}"
            );
        }

        // The threads metadata root itself is rejected (never a checkout slot).
        validate_worktree_target(&repo, &threads_root, None)
            .expect_err("the .heddle/threads metadata root must be rejected");

        // The bare per-thread directory (no `root` leaf) is rejected: the
        // manifest sidecar lives there, so a checkout would swallow it
        // (heddle#572 r3 Finding #4).
        let err = validate_worktree_target(&repo, &threads_root.join("brandnew"), None)
            .expect_err("a bare .heddle/threads/<name> dir (no leaf) must be rejected");
        assert!(
            err.to_string().contains("per-thread checkout leaf"),
            "unexpected error for the no-leaf target: {err}"
        );

        // A fresh per-thread checkout LEAF is accepted.
        validate_worktree_target(&repo, &threads_root.join("brandnew").join("root"), None)
            .expect("a fresh .heddle/threads/<name>/root checkout is allowed");

        // Finding #2: an EXISTING thread's OWN canonical `root` is reserved.
        // A *different* caller (e.g. a fresh `start other`) must NOT reuse it,
        // but a same-thread promote/re-materialize of `foo` may.
        validate_worktree_target(&repo, &threads_root.join("foo").join("root"), None)
            .expect_err("another thread must not reuse foo's reserved root");
        validate_worktree_target(&repo, &threads_root.join("foo").join("root"), Some("foo"))
            .expect("re-materializing foo's own canonical root slot is allowed");

        // Finding #1: a thread checked out at a CUSTOM `--path` reserves that
        // recorded location too — a later `--path` nested under it is rejected
        // even though it lies outside the thread's canonical `thread_dir`.
        let custom_root = threads_root.join("custom-slot").join("root");
        register_thread_at(
            &repo,
            "custom",
            repo::ThreadMode::Solid,
            custom_root.clone(),
        );
        let nested_in_custom = custom_root.join("nested");
        validate_worktree_target(&repo, &nested_in_custom, None)
            .expect_err("a path nested in a thread's recorded custom checkout must be rejected");
        // The custom thread itself may re-materialize at its own recorded root.
        validate_worktree_target(&repo, &custom_root, Some("custom"))
            .expect("the same thread may re-use its own recorded checkout root");
        // But another caller may not reuse it.
        validate_worktree_target(&repo, &custom_root, None)
            .expect_err("another thread must not reuse a recorded custom checkout root");
    }

    /// The same chokepoint still materializes the real bytes for a state that
    /// IS visible to the operator-local audience — the gate fails closed only
    /// for under-tier states.
    #[test]
    fn write_isolated_checkout_materializes_visible_state() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        std::fs::write(repo_dir.path().join("ok.rs"), b"fn ok() {}\n").unwrap();
        repo.snapshot(Some("public".into()), None).unwrap();
        let state_id = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("head present");

        let holder = TempDir::new().unwrap();
        let dest = holder.path().join("out");
        write_isolated_checkout(&repo, &dest, &state_id, Some("main")).expect("checkout");

        assert!(
            dest.join("ok.rs").exists(),
            "a visible state materializes its real bytes"
        );
        assert!(
            !dest.join(COURTESY_STUB_FILENAME).exists(),
            "no courtesy stub for a visible state"
        );
    }
}
