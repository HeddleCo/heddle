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

use chrono::{DateTime, Utc};
use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
    legacy,
    store::ObjectStore,
};
use oplog::OpLogBackend;
use serde::{Deserialize, Serialize};

use crate::{
    Repository,
    repository::repo_config,
    thread_model::{
        EphemeralMarker, ThreadConfidenceSummary, ThreadFreshness, ThreadImpactCategory,
        ThreadIntegrationPolicy, ThreadMode, ThreadRecord, ThreadState, ThreadVerificationSummary,
    },
    thread_record_store::FilesystemThreadRecordStore,
};

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
    Trees,
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
        id: "0003_canonicalize_tree_entries",
        // Load-bearing for strict Tree V2 reads: Repository::open must keep
        // running this before any caller can touch tree objects, otherwise old
        // V1 tree bytes become arbitrary runtime read failures.
        description: "Rewrite removed V1 tree entries into the current first-class gitlink tree envelope",
        applies_to: SchemaTarget::Trees,
        run: run_canonicalize_tree_entries,
    },
    Migration {
        id: "0006_canonicalize_packed_oplog",
        description: "Rewrite packed oplog files into the latest container and record schema",
        applies_to: SchemaTarget::OpLog,
        run: run_canonicalize_packed_oplog,
    },
];

/// Returns true when every registered migration id is already recorded in
/// the schema ledger. Used by repository open to skip the migration pass
/// entirely on a clean ledger (see `docs/perf/cli-core-loop-todo.md`).
///
/// Missing or malformed ledger → `false` (caller must run
/// [`apply_pending`]). Malformed ledger is *not* surfaced as an error here:
/// open still routes through `apply_pending`, which reports the parse failure.
pub fn is_schema_ledger_complete(heddle_dir: &Path) -> bool {
    let path = heddle_dir.join("state/schema_versions.toml");
    let Ok(ledger) = SchemaVersionsLedger::load(&path) else {
        return false;
    };
    MIGRATIONS
        .iter()
        .all(|migration| ledger.applied.contains(migration.id))
}

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
        // Hard gate, not telemetry: a failed migration means the repo may still
        // contain bytes that current strict readers reject. Do not downgrade
        // this to warn-and-continue.
        (migration.run)(&mut ctx).map_err(|err| {
            HeddleError::InvalidObject(format!(
                "migration {} ({}) failed: {err}",
                migration.id, migration.description
            ))
        })?;
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
    let store = FilesystemThreadRecordStore::new(ctx.repo.heddle_dir().join("thread_records"));
    if !store.root().exists() {
        return Ok(());
    }
    for dir_entry in std::fs::read_dir(store.root())? {
        let path = dir_entry?.path();
        if !path.extension().map(|ext| ext == "toml").unwrap_or(false) {
            continue;
        }
        let content = std::fs::read_to_string(&path)?;
        let legacy: LegacyThreadRecord = toml::from_str(&content).map_err(|err| {
            HeddleError::InvalidObject(format!(
                "thread record {} is malformed and cannot be migrated: {err}",
                path.display()
            ))
        })?;
        let record: ThreadRecord = legacy.into();
        store.save_record(&record)?;
    }
    Ok(())
}

fn run_canonicalize_packed_oplog(ctx: &mut MigrationCtx<'_>) -> Result<()> {
    ctx.repo.oplog().migrate_to_current_format()
}

fn run_canonicalize_tree_entries(ctx: &mut MigrationCtx<'_>) -> Result<()> {
    for tree_hash in ctx.repo.store().list_trees()? {
        let Some(raw) = ctx.repo.store().get_tree_serialized(&tree_hash)? else {
            continue;
        };

        if let Ok(tree) = rmp_serde::from_slice::<objects::object::Tree>(&raw) {
            tree.validate()?;
            let found = tree.hash();
            if found != tree_hash {
                return Err(HeddleError::Corruption {
                    expected: tree_hash,
                    found,
                });
            }
            continue;
        }

        let tree = legacy::decode_legacy_tree_v1(&raw).map_err(|err| {
            HeddleError::InvalidObject(format!(
                "failed to decode legacy V1 tree {} during 0003_canonicalize_tree_entries: {err}",
                tree_hash.short()
            ))
        })?;
        let found = tree.hash();
        if found != tree_hash {
            return Err(HeddleError::Corruption {
                expected: tree_hash,
                found,
            });
        }
        let canonical = rmp_serde::to_vec(&tree).map_err(|err| {
            HeddleError::InvalidObject(format!(
                "failed to encode canonical V2 tree {} during 0003_canonicalize_tree_entries: {err}",
                tree_hash.short()
            ))
        })?;
        ctx.repo
            .store()
            .put_tree_serialized(&canonical, tree_hash)
            .map_err(|err| {
                HeddleError::InvalidObject(format!(
                    "failed to write canonical V2 tree {} during 0003_canonicalize_tree_entries: {err}",
                    tree_hash.short()
                ))
            })?;
    }

    bump_repo_format_to_supported(ctx.repo)
}

fn bump_repo_format_to_supported(repo: &Repository) -> Result<()> {
    let config_path = repo.heddle_dir().join("config.toml");
    let mut config = repo_config::RepoConfig::load(&config_path)?;
    if config.repository.version < repo_config::SUPPORTED_REPO_FORMAT {
        config.repository.version = repo_config::SUPPORTED_REPO_FORMAT;
        config.save(&config_path)?;
    }
    Ok(())
}

/// Decode-only shape for `0002_canonicalize_thread_records`.
///
/// This is intentionally private to the migration: live `ThreadRecord` loading
/// stays strict, while the one-shot gate can still read pre-gate records and
/// rewrite them into the current durable schema.
#[derive(Debug, Deserialize)]
struct LegacyThreadRecord {
    id: String,
    thread: String,
    #[serde(default)]
    target_thread: Option<String>,
    #[serde(default)]
    parent_thread: Option<String>,
    mode: ThreadMode,
    state: ThreadState,
    base_state: String,
    base_root: String,
    #[serde(default)]
    current_state: Option<String>,
    #[serde(default)]
    merged_state: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    changed_paths: Vec<String>,
    #[serde(default)]
    impact_categories: Vec<ThreadImpactCategory>,
    #[serde(default)]
    heavy_impact_paths: Vec<String>,
    #[serde(default)]
    promotion_suggested: bool,
    #[serde(default = "legacy_thread_freshness")]
    freshness: ThreadFreshness,
    #[serde(default)]
    verification_summary: ThreadVerificationSummary,
    #[serde(default)]
    confidence_summary: ThreadConfidenceSummary,
    #[serde(default)]
    integration_policy_result: ThreadIntegrationPolicy,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    ephemeral: Option<EphemeralMarker>,
    #[serde(default)]
    auto: bool,
    #[serde(default)]
    shared_target_dir: Option<PathBuf>,
}

fn legacy_thread_freshness() -> ThreadFreshness {
    ThreadFreshness::Unknown
}

impl From<LegacyThreadRecord> for ThreadRecord {
    fn from(record: LegacyThreadRecord) -> Self {
        Self {
            id: record.id,
            thread: record.thread,
            target_thread: record.target_thread,
            parent_thread: record.parent_thread,
            mode: record.mode,
            state: record.state,
            base_state: record.base_state,
            base_root: record.base_root,
            current_state: record.current_state,
            merged_state: record.merged_state,
            task: record.task,
            changed_paths: record.changed_paths,
            impact_categories: record.impact_categories,
            heavy_impact_paths: record.heavy_impact_paths,
            promotion_suggested: record.promotion_suggested,
            freshness: record.freshness,
            verification_summary: record.verification_summary,
            confidence_summary: record.confidence_summary,
            integration_policy_result: record.integration_policy_result,
            created_at: record.created_at,
            updated_at: record.updated_at,
            ephemeral: record.ephemeral,
            auto: record.auto,
            shared_target_dir: record.shared_target_dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Mutex};

    use crypto::StateSigningExt;
    use objects::object::{
        Annotation, AnnotationKind, AnnotationScope, Attribution, Blob, ContentHash, ContextBlob,
        ContextTarget, EntryType, Principal, SignatureStatus, State, Tree, TreeEntry,
    };
    use serde::Serialize;
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

    fn loose_tree_path(repo: &Repository, hash: ContentHash) -> PathBuf {
        let hex = hash.to_hex();
        let (prefix, rest) = hex.split_at(2);
        repo.heddle_dir()
            .join("objects")
            .join("trees")
            .join(prefix)
            .join(rest)
    }

    fn write_loose_tree_bytes(repo: &Repository, hash: ContentHash, bytes: &[u8]) {
        let path = loose_tree_path(repo, hash);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[derive(Serialize)]
    struct LegacyTreeV1ForTest {
        entries: Vec<LegacyTreeEntryV1ForTest>,
    }

    #[derive(Serialize)]
    struct LegacyTreeEntryV1ForTest {
        name: String,
        mode: LegacyFileModeForTest,
        entry_type: LegacyEntryTypeForTest,
        hash: ContentHash,
    }

    #[derive(Serialize)]
    enum LegacyFileModeForTest {
        Normal,
    }

    #[derive(Serialize)]
    enum LegacyEntryTypeForTest {
        Blob,
    }

    fn legacy_tree_v1_bytes(name: &str, hash: ContentHash) -> Vec<u8> {
        rmp_serde::to_vec(&LegacyTreeV1ForTest {
            entries: vec![LegacyTreeEntryV1ForTest {
                name: name.to_string(),
                mode: LegacyFileModeForTest::Normal,
                entry_type: LegacyEntryTypeForTest::Blob,
                hash,
            }],
        })
        .unwrap()
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
    fn migrations_are_registered_in_order() {
        let registered = MIGRATIONS
            .iter()
            .map(|migration| migration.id)
            .collect::<Vec<_>>();

        assert_eq!(
            registered,
            vec![
                "0002_canonicalize_thread_records",
                "0003_canonicalize_tree_entries",
                "0006_canonicalize_packed_oplog",
            ],
            "the deletion-wave migration ids must stay ordered and reviewable",
        );
        assert!(NEXT_DELETION_WAVE_MIGRATIONS.is_empty());
    }

    #[test]
    fn migration_0003_rewrites_v1_tree_bytes_and_preserves_marker_blob() {
        let (_temp, repo) = fresh_repo();
        remove_ledger(&repo);
        let config_path = repo.heddle_dir().join("config.toml");
        let mut config = repo_config::RepoConfig::load(&config_path).unwrap();
        config.repository.version = 1;
        config.save(&config_path).unwrap();

        let marker = b"heddle-submodule: 0808080808080808080808080808080808080808";
        let blob_hash = repo.store().put_blob(&Blob::from_slice(marker)).unwrap();
        let current_tree =
            Tree::from_entries(vec![TreeEntry::file("vendor", blob_hash, false).unwrap()]);
        let tree_hash = current_tree.hash();
        let legacy_tree_bytes = legacy_tree_v1_bytes("vendor", blob_hash);
        write_loose_tree_bytes(&repo, tree_hash, &legacy_tree_bytes);

        assert!(
            repo.store().get_tree(&tree_hash).is_err(),
            "current runtime reader must reject legacy V1 tree bytes before migration"
        );
        assert_eq!(
            repo.store()
                .get_tree_serialized(&tree_hash)
                .unwrap()
                .expect("raw legacy tree exists"),
            legacy_tree_bytes,
            "migration raw-read seam must not use current Tree deserialization"
        );

        apply_pending(&repo).unwrap();

        let decoded = repo
            .store()
            .get_tree(&tree_hash)
            .unwrap()
            .expect("tree exists after migration");
        let entry = decoded.get("vendor").expect("entry preserved");
        assert!(entry.is_blob());
        assert_eq!(entry.blob_hash(), Some(blob_hash));
        assert!(entry.gitlink_target().is_none());

        let raw_after = repo
            .store()
            .get_tree_serialized(&tree_hash)
            .unwrap()
            .expect("raw tree exists after migration");
        rmp_serde::from_slice::<Tree>(&raw_after).expect("tree is canonical V2 after migration");

        let config = repo_config::RepoConfig::load(&config_path).unwrap();
        assert_eq!(
            config.repository.version,
            repo_config::SUPPORTED_REPO_FORMAT
        );
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
}
