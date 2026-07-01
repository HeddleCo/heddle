// SPDX-License-Identifier: Apache-2.0
//! Declarative repository migrations.
//!
//! A single registered, ordered list of forward-only migrations applied on
//! repo open. `apply_pending` walks the list and runs anything missing from
//! `.heddle/state/schema_versions.toml`.
//!
//! # Adding a migration
//!
//! 1. Pick a stable id (`NNNN_short_description`) — they sort lexicographically
//!    and the registry runs in that order. Reserve a fresh four-digit prefix.
//! 2. Add an entry to [`MIGRATIONS`] with a `run` closure that performs the
//!    one-shot transformation.
//! 3. The migration must be idempotent: if the target state is already
//!    correct, `run` should detect that and return `Ok(())` without doing
//!    anything destructive.
//!
//! # Why declarative
//!
//! The previous pattern was `Repository::open` calling individual fix-up
//! functions inline. That worked for one or two migrations and started to
//! tangle once W1 introduced four new persisted blob types, an oplog actor
//! field, an annotation visibility field, and the operation log index.
//! Centralizing the registry gives us:
//!
//! - A single ordered audit trail of every schema transition.
//! - One place to instrument timing / logging / telemetry.
//! - Idempotent re-runs — applying twice is a no-op.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use crypto::verify_state_signature_bytes;
use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
    store::ObjectStore,
};
use serde::{Deserialize, Serialize};

use crate::{Repository, ResignOutcome, thread_storage::ThreadManager};

/// One declarative migration. The `run` closure receives a [`MigrationCtx`]
/// so it can read the repo and persist any changes; it returns `Ok(())` on
/// success and a `HeddleError` to abort `apply_pending`.
#[derive(Clone)]
pub struct Migration {
    /// Stable id, sorted lexicographically. Convention: `NNNN_description`.
    pub id: &'static str,
    /// One-line description shown in logs/telemetry.
    pub description: &'static str,
    /// Coarse target the migration touches. Multiple migrations may share
    /// the same target.
    pub applies_to: SchemaTarget,
    /// The migration body. Must be idempotent.
    pub run: fn(&mut MigrationCtx) -> Result<()>,
}

/// A future migration that is intentionally NOT registered in [`MIGRATIONS`]
/// yet. These entries reserve the deletion-wave hooks and name the safety gate
/// that must be satisfied before the runtime backcompat reader can be removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedDeletionMigration {
    pub id: &'static str,
    pub description: &'static str,
    pub applies_to: SchemaTarget,
    pub safe_to_register_when: &'static str,
}

/// Coarse-grained subsystem a migration touches. Useful for logging and for
/// future per-target conditional skip logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaTarget {
    OpLog,
    ContextBlobs,
    ThreadRecords,
    OperationIndex,
    RefSummary,
    PullPlannerCache,
    ColdCloneManifest,
    /// Catch-all for migrations that touch multiple stores.
    Mixed,
}

/// Mutable context passed to a migration's `run` closure. Owns a borrow of
/// the [`Repository`] so migrations can drive ref/oplog work through the
/// usual public surface.
pub struct MigrationCtx<'a> {
    pub repo: &'a Repository,
}

/// Reserved hooks for the next legacy-deletion wave. Keep this list in sync
/// with `docs/LEGACY_DELETION_NEXT_WAVE.md`.
pub const NEXT_DELETION_WAVE_MIGRATIONS: &[PlannedDeletionMigration] = &[];

/// Per-migration outcome. Returned by [`apply_pending`] for telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationOutcome {
    pub id: &'static str,
    pub status: MigrationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationStatus {
    /// Migration was missing from the ledger and ran successfully.
    Applied,
    /// Migration was already in the ledger and was skipped.
    AlreadyApplied,
}

/// Aggregate report of a migration pass. Always lists every registered
/// migration so callers can see the full state, not just the deltas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub outcomes: Vec<MigrationOutcome>,
}

impl MigrationReport {
    pub fn applied(&self) -> impl Iterator<Item = &str> {
        self.outcomes.iter().filter_map(|o| match o.status {
            MigrationStatus::Applied => Some(o.id),
            MigrationStatus::AlreadyApplied => None,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.outcomes.is_empty()
    }
}

/// Persisted ledger of applied migrations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SchemaVersionsLedger {
    #[serde(default)]
    applied: BTreeSet<String>,
}

impl SchemaVersionsLedger {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path).map_err(HeddleError::from)?;
        toml::from_str(&raw).map_err(|err| {
            HeddleError::InvalidObject(format!(
                "schema_versions.toml at {} is malformed: {err}",
                path.display()
            ))
        })
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(HeddleError::from)?;
        }
        let raw = toml::to_string_pretty(self).map_err(|err| {
            HeddleError::InvalidObject(format!("failed to serialize schema_versions.toml: {err}"))
        })?;
        write_file_atomic(path, raw.as_bytes())?;
        Ok(())
    }
}

fn ledger_path(repo: &Repository) -> PathBuf {
    repo.heddle_dir().join("state/schema_versions.toml")
}

/// Registered migrations, run in this order. New migrations append at the
/// tail; never reorder existing ids.
///
/// `0001_legacy_tracks` is intentionally retired with no registered body:
/// it covered a pre-v0.2.0 ref layout that was removed from the runtime.
/// The deletion-prep migrations after it make durable data canonical before
/// their runtime fallbacks are removed.
pub static MIGRATIONS: &[Migration] = &[
    Migration {
        id: "0002_canonicalize_thread_records",
        description: "Rewrite durable thread records so readers no longer need serde defaults",
        applies_to: SchemaTarget::ThreadRecords,
        run: run_canonicalize_thread_records,
    },
    Migration {
        id: "0003_canonicalize_context_roots",
        description: "Rewrite legacy direct-path context roots without breaking signed states",
        applies_to: SchemaTarget::ContextBlobs,
        run: run_canonicalize_context_roots,
    },
    Migration {
        id: "0004_resecure_pre_fidelity_signatures",
        description: "Backfill pre-fidelity state signatures onto current hashes",
        applies_to: SchemaTarget::Mixed,
        run: run_resecure_pre_fidelity_signatures,
    },
];

/// Apply any registered migration not yet present in
/// `<repo>/.heddle/state/schema_versions.toml`. Idempotent: a second
/// invocation produces zero `Applied` outcomes.
pub fn apply_pending(repo: &Repository) -> Result<MigrationReport> {
    let ledger_file = ledger_path(repo);
    let mut ledger = SchemaVersionsLedger::load(&ledger_file)?;
    let mut outcomes = Vec::with_capacity(MIGRATIONS.len());
    let mut newly_applied = Vec::new();

    for migration in MIGRATIONS {
        if ledger.applied.contains(migration.id) {
            outcomes.push(MigrationOutcome {
                id: migration.id,
                status: MigrationStatus::AlreadyApplied,
            });
            continue;
        }
        let mut ctx = MigrationCtx { repo };
        (migration.run)(&mut ctx)?;
        newly_applied.push(migration.id.to_string());
        outcomes.push(MigrationOutcome {
            id: migration.id,
            status: MigrationStatus::Applied,
        });
    }

    if !newly_applied.is_empty() {
        ledger.applied.extend(newly_applied);
        ledger.save(&ledger_file)?;
    }

    Ok(MigrationReport { outcomes })
}

fn run_canonicalize_thread_records(ctx: &mut MigrationCtx<'_>) -> Result<()> {
    let manager = ThreadManager::new(ctx.repo.heddle_dir());
    for record in manager.list_records()? {
        manager.save_record(&record)?;
    }
    Ok(())
}

fn run_canonicalize_context_roots(ctx: &mut MigrationCtx<'_>) -> Result<()> {
    let mut blocked = Vec::new();

    for state_id in ctx.repo.store().list_states()? {
        let Some(state) = ctx.repo.store().get_state(&state_id)? else {
            continue;
        };
        let Some(context_root) = state.context else {
            continue;
        };
        let (canonical_root, changed) = ctx.repo.canonicalize_context_root(&context_root)?;
        if !changed {
            continue;
        }

        let prior_hash = state.compute_hash();
        let prior_pre_fidelity_hash = state.compute_hash_pre_fidelity();
        let mut rewritten = state.with_context(canonical_root);

        match ctx
            .repo
            .resign_if_owned(&mut rewritten, &[prior_hash, prior_pre_fidelity_hash])
        {
            ResignOutcome::Unsigned | ResignOutcome::Resigned => {
                ctx.repo.store().put_state(&rewritten)?;
            }
            ResignOutcome::Unreproducible => {
                blocked.push(state_id);
            }
        }
    }

    if blocked.is_empty() {
        Ok(())
    } else {
        Err(HeddleError::Conflict(format!(
            "0003_canonicalize_context_roots left {} signed state(s) with unreproducible legacy direct-path context roots; keep the legacy context fallback or migrate them with the owning key",
            blocked.len()
        )))
    }
}

fn run_resecure_pre_fidelity_signatures(ctx: &mut MigrationCtx<'_>) -> Result<()> {
    let mut blocked = Vec::new();

    for state_id in ctx.repo.store().list_states()? {
        let Some(mut state) = ctx.repo.store().get_state(&state_id)? else {
            continue;
        };
        let Some(signature) = state.signature.clone() else {
            continue;
        };
        let current_hash = state.compute_hash();
        if verify_state_signature_bytes(&signature, &current_hash).is_ok() {
            continue;
        }
        let pre_fidelity_hash = state.compute_hash_pre_fidelity();
        if verify_state_signature_bytes(&signature, &pre_fidelity_hash).is_err() {
            continue;
        }

        match ctx
            .repo
            .resign_if_owned(&mut state, &[current_hash, pre_fidelity_hash])
        {
            ResignOutcome::Resigned => ctx.repo.store().put_state(&state)?,
            ResignOutcome::Unsigned => {}
            ResignOutcome::Unreproducible => blocked.push(state_id),
        }
    }

    if blocked.is_empty() {
        Ok(())
    } else {
        Err(HeddleError::Conflict(format!(
            "0004_resecure_pre_fidelity_signatures found {} valid pre-fidelity signature(s) whose key is not owned by this repo; keep compute_hash_pre_fidelity until they are explicitly preserved or re-signed by the owning key",
            blocked.len()
        )))
    }
}
#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crypto::StateSigningExt;
    use objects::object::{
        Annotation, AnnotationKind, AnnotationScope, Attribution, Blob, ContentHash, ContextBlob,
        ContextTarget, EntryType, Principal, SignatureStatus, State, Tree, TreeEntry,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::{Repository, thread_record_store::FilesystemThreadRecordStore};

    static SIGNING_HOME_LOCK: Mutex<()> = Mutex::new(());

    fn fresh_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    fn with_signing_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = SIGNING_HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("HEDDLE_HOME");
        unsafe {
            std::env::set_var("HEDDLE_HOME", home);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match previous {
            Some(value) => unsafe { std::env::set_var("HEDDLE_HOME", value) },
            None => unsafe { std::env::remove_var("HEDDLE_HOME") },
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn remove_ledger(repo: &Repository) {
        let ledger_file = ledger_path(repo);
        if ledger_file.exists() {
            std::fs::remove_file(&ledger_file).unwrap();
        }
    }

    fn test_annotation(content: &str) -> Annotation {
        Annotation::new(
            AnnotationScope::File,
            AnnotationKind::Rationale,
            content.to_string(),
            vec![],
            "test@example.com".to_string(),
            1700000000,
            None,
            None,
        )
    }

    fn tree_path(repo: &Repository, components: &[String], blob_hash: ContentHash) -> ContentHash {
        let mut tree = Tree::new();
        if components.len() == 1 {
            tree.insert(TreeEntry::file(&components[0], blob_hash, false).unwrap());
        } else {
            let subtree = tree_path(repo, &components[1..], blob_hash);
            tree.insert(TreeEntry::directory(&components[0], subtree).unwrap());
        }
        repo.store().put_tree(&tree).unwrap()
    }

    fn legacy_context_root(repo: &Repository, path: &str, blob: &ContextBlob) -> ContentHash {
        let bytes = blob.encode().unwrap();
        let blob_hash = repo.store().put_blob(&Blob::new(bytes)).unwrap();
        let components = Path::new(path)
            .components()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        tree_path(repo, &components, blob_hash)
    }

    fn unsigned_state() -> State {
        State::new_snapshot(
            ContentHash::compute(b"tree"),
            vec![],
            Attribution::human(Principal::new("Test", "test@example.com")),
        )
    }

    #[test]
    fn first_apply_runs_every_registered_migration() {
        let (_temp, repo) = fresh_repo();
        // Clear any auto-applied state from `init_default` so we have a
        // clean comparison.
        remove_ledger(&repo);
        let report = apply_pending(&repo).unwrap();
        let applied: Vec<&str> = report.applied().collect();
        assert_eq!(applied.len(), MIGRATIONS.len());
    }

    #[test]
    fn second_apply_is_a_no_op() {
        let (_temp, repo) = fresh_repo();
        remove_ledger(&repo);
        apply_pending(&repo).unwrap();
        let report = apply_pending(&repo).unwrap();
        assert!(report.applied().next().is_none());
        assert_eq!(report.outcomes.len(), MIGRATIONS.len());
    }

    #[test]
    fn ledger_persists_applied_ids() {
        let (_temp, repo) = fresh_repo();
        let ledger_file = ledger_path(&repo);
        remove_ledger(&repo);
        apply_pending(&repo).unwrap();
        let raw = std::fs::read_to_string(&ledger_file).unwrap();
        for migration in MIGRATIONS {
            assert!(
                raw.contains(migration.id),
                "missing {} in ledger",
                migration.id
            );
        }
    }

    #[test]
    fn deletion_wave_hooks_are_registered_in_order() {
        let registered = MIGRATIONS
            .iter()
            .map(|migration| migration.id)
            .collect::<Vec<_>>();

        assert_eq!(
            registered,
            vec![
                "0002_canonicalize_thread_records",
                "0003_canonicalize_context_roots",
                "0004_resecure_pre_fidelity_signatures",
            ],
            "the deletion-wave migration ids must stay ordered and reviewable",
        );
        assert!(NEXT_DELETION_WAVE_MIGRATIONS.is_empty());
    }

    #[test]
    fn migration_0002_rewrites_minimal_thread_record_with_concrete_defaults() {
        let (_temp, repo) = fresh_repo();
        remove_ledger(&repo);
        let store = FilesystemThreadRecordStore::new(repo.heddle_dir().join("thread_records"));
        let path = store.record_path("legacy-minimal").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"
id = "legacy-minimal"
thread = "legacy/minimal"
mode = "solid"
state = "active"
base_state = "abc123"
base_root = "def456"
created_at = "2024-01-01T00:00:00Z"
updated_at = "2024-01-01T00:00:01Z"
"#,
        )
        .unwrap();

        apply_pending(&repo).unwrap();

        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(rewritten.contains("changed_paths = []"));
        assert!(rewritten.contains("impact_categories = []"));
        assert!(rewritten.contains("heavy_impact_paths = []"));
        assert!(rewritten.contains("promotion_suggested = false"));
        assert!(rewritten.contains("freshness = \"unknown\""));
        assert!(rewritten.contains("auto = false"));
        assert!(rewritten.contains("[verification_summary]"));
        assert!(rewritten.contains("[confidence_summary]"));
        assert!(rewritten.contains("[integration_policy_result]"));
        assert!(rewritten.contains("conflicts_resolved_manually = false"));
    }

    #[test]
    fn migration_0003_canonicalizes_owned_signed_context_root_and_resigns() {
        let home = TempDir::new().unwrap();
        with_signing_home(home.path(), || {
            let temp = TempDir::new().unwrap();
            let repo = Repository::init(temp.path()).unwrap();
            remove_ledger(&repo);
            let target = ContextTarget::file("src/main.rs").unwrap();
            let blob = ContextBlob::new(vec![test_annotation("legacy")]);
            let legacy_root = legacy_context_root(&repo, "src/main.rs", &blob);

            let signer = repo.signing_signer().unwrap();
            let mut state = unsigned_state().with_context(legacy_root);
            state.sign(&*signer).unwrap();
            let state_id = state.change_id;
            repo.store().put_state(&state).unwrap();

            apply_pending(&repo).unwrap();

            let migrated = repo.store().get_state(&state_id).unwrap().unwrap();
            assert_ne!(migrated.context, Some(legacy_root));
            assert_eq!(
                repo.verify_state_signature(&state_id).unwrap(),
                SignatureStatus::Valid
            );
            let new_root = migrated.context.unwrap();
            assert_eq!(
                repo.get_context_blob(&new_root, &target).unwrap(),
                Some(blob)
            );
            let top = repo.store().get_tree(&new_root).unwrap().unwrap();
            assert!(top.get("src").is_none());
            assert_eq!(
                top.get("__files").map(|entry| entry.entry_type),
                Some(EntryType::Tree),
            );
        });
    }

    #[test]
    fn migration_0004_resigns_owned_pre_fidelity_signature() {
        let home = TempDir::new().unwrap();
        with_signing_home(home.path(), || {
            let temp = TempDir::new().unwrap();
            let repo = Repository::init(temp.path()).unwrap();
            remove_ledger(&repo);

            let signer = repo.signing_signer().unwrap();
            let mut state = unsigned_state();
            let legacy_hash = state.compute_hash_pre_fidelity();
            state.signature = Some(
                crypto::state_signature_from_signer(&legacy_hash, &*signer)
                    .expect("sign legacy hash"),
            );
            let state_id = state.change_id;
            repo.store().put_state(&state).unwrap();
            assert_eq!(
                repo.verify_state_signature(&state_id).unwrap(),
                SignatureStatus::Invalid,
            );

            apply_pending(&repo).unwrap();

            assert_eq!(
                repo.verify_state_signature(&state_id).unwrap(),
                SignatureStatus::Valid,
            );
        });
    }

    #[test]
    fn migration_0004_refuses_to_mark_foreign_pre_fidelity_signature_complete() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        remove_ledger(&repo);
        let foreign = crypto::Ed25519Signer::generate().unwrap();
        let mut state = unsigned_state();
        let legacy_hash = state.compute_hash_pre_fidelity();
        state.signature = Some(
            crypto::state_signature_from_signer(&legacy_hash, &foreign)
                .expect("foreign-sign legacy hash"),
        );
        repo.store().put_state(&state).unwrap();

        let err = apply_pending(&repo).expect_err("foreign legacy signature blocks 0004");
        assert!(
            err.to_string()
                .contains("0004_resecure_pre_fidelity_signatures"),
            "{err}"
        );
        let ledger_file = ledger_path(&repo);
        if ledger_file.exists() {
            let ledger = std::fs::read_to_string(&ledger_file).unwrap();
            assert!(!ledger.contains("0004_resecure_pre_fidelity_signatures"));
        }
    }
}
