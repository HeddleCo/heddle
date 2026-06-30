// SPDX-License-Identifier: Apache-2.0
//! Declarative repository migrations.
//!
//! A single registered, ordered list of forward-only migrations applied on
//! repo open. `apply_pending` walks the list and runs anything missing from
//! `.heddle/state/schema_versions.toml`. The list is currently empty (a clean
//! no-op); the framework stays so future schema changes have a place to land
//! without tangling `Repository::open_raw`/`open`.
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

use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
};
use serde::{Deserialize, Serialize};

use crate::Repository;

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
/// Currently empty: the framework exists so future schema changes have a
/// place to land without tangling `Repository::open_raw`. The former
/// `0001_legacy_tracks` migration (which renamed a pre-v0.2.0 `tracks/`
/// layout that was never publicly produced) was removed — `apply_pending`
/// over an empty list is a clean no-op.
pub static MIGRATIONS: &[Migration] = &[];

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

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::Repository;

    fn fresh_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    #[test]
    fn first_apply_runs_every_registered_migration() {
        let (_temp, repo) = fresh_repo();
        // Clear any auto-applied state from `init_default` so we have a
        // clean comparison.
        let ledger_file = ledger_path(&repo);
        if ledger_file.exists() {
            std::fs::remove_file(&ledger_file).unwrap();
        }
        let report = apply_pending(&repo).unwrap();
        let applied: Vec<&str> = report.applied().collect();
        assert_eq!(applied.len(), MIGRATIONS.len());
    }

    #[test]
    fn second_apply_is_a_no_op() {
        let (_temp, repo) = fresh_repo();
        let ledger_file = ledger_path(&repo);
        if ledger_file.exists() {
            std::fs::remove_file(&ledger_file).unwrap();
        }
        apply_pending(&repo).unwrap();
        let report = apply_pending(&repo).unwrap();
        assert!(report.applied().next().is_none());
        assert_eq!(report.outcomes.len(), MIGRATIONS.len());
    }

    #[test]
    fn ledger_persists_applied_ids() {
        let (_temp, repo) = fresh_repo();
        let ledger_file = ledger_path(&repo);
        if ledger_file.exists() {
            std::fs::remove_file(&ledger_file).unwrap();
        }
        let report = apply_pending(&repo).unwrap();
        let applied: Vec<&str> = report.applied().collect();
        // Every id that `apply_pending` reports as Applied must be durable in
        // the ledger. When the registry is empty nothing is applied and no
        // ledger is written — that is the correct no-op.
        if applied.is_empty() {
            assert!(!ledger_file.exists());
        } else {
            let raw = std::fs::read_to_string(&ledger_file).unwrap();
            for id in applied {
                assert!(raw.contains(id), "missing {id} in ledger");
            }
        }
    }
}
