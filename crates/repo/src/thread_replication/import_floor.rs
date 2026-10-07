//! Durable record of a HYBRID import floor: the converted Git ancestors below
//! one delegated import tip (HeddleCo/heddle#2004, api alpha.42).
//!
//! An import is one signed native operation per branch; its Git ancestors are
//! States in the tip State's parent closure, never operations. Once a Fetch
//! has verified and installed those States, this table remembers which tip
//! they belong to so that
//! - the local visibility walk stops at the import tip, as the host's
//!   lineage does, instead of walking every converted commit or giving up
//!   at its bound and withholding the whole checkout;
//! - a lazily fetched older commit can record source possession;
//! - a later Fetch of the same Thread can tell the endpoint it already holds
//!   the floor (`TransferSelection.exclude_revisions`).
//!
//! Membership is derived here from the installed States by walking the tip's
//! parents down to the signed causal frontier. It is never copied from the
//! wire: the pages only prove the States exist; the store holds them.
use std::collections::{BTreeSet, VecDeque};

use objects::{
    object::{ContentHash, State, StateId},
    store::ObjectStore,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{Error, Result};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS import_floors(
 thread BLOB NOT NULL CHECK(length(thread)=32),tip BLOB NOT NULL CHECK(length(tip)=32),
 operation BLOB NOT NULL CHECK(length(operation)=32),PRIMARY KEY(thread,tip));
CREATE TABLE IF NOT EXISTS import_floor_members(
 thread BLOB NOT NULL,tip BLOB NOT NULL,member BLOB NOT NULL CHECK(length(member)=32),
 PRIMARY KEY(thread,tip,member),
 FOREIGN KEY(thread,tip) REFERENCES import_floors(thread,tip) ON DELETE CASCADE);
CREATE INDEX IF NOT EXISTS import_floor_members_member ON import_floor_members(member);";

/// Hard bound on one floor's membership walk. Double the api contract floor
/// (131072) so boost's 94794 commits fit; a larger import is refused at
/// Prepare, not truncated here.
pub const MAX_IMPORT_FLOOR_STATES: usize = 262_144;

/// How a State relates to recorded import floors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportFloorRole {
    /// The tip of a recorded floor: its converted ancestry is covered by the
    /// signed import, so a walk need not descend.
    Tip,
    /// A converted ancestor inside the floor of `tip`.
    Member { tip: StateId },
}

pub(crate) fn initialize_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(SCHEMA)
}

/// Whether `(thread, tip)` already has a recorded floor.
pub(super) fn recorded_in(tx: &Transaction<'_>, thread: ContentHash, tip: StateId) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM import_floors WHERE thread=?1 AND tip=?2)",
        params![thread.as_bytes(), tip.as_bytes()],
        |row| row.get(0),
    )?)
}

/// Walk the installed converted ancestry below `tip`, stopping at the signed
/// frontier, and record it. Every member must already be in `store` with its
/// address intact; an absent or mislabeled State fails the installation so a
/// clone never settles with only the tip (the embargo-placeholder failure).
pub(super) fn record_in(
    tx: &Transaction<'_>,
    thread: ContentHash,
    operation: ContentHash,
    tip: &State,
    frontier: &BTreeSet<StateId>,
    store: &impl ObjectStore,
) -> Result<usize> {
    let tip_id = tip.id();
    let mut members = BTreeSet::new();
    let mut pending: VecDeque<StateId> = tip
        .parents
        .iter()
        .copied()
        .filter(|parent| !frontier.contains(parent))
        .collect();
    while let Some(id) = pending.pop_front() {
        if id == tip_id || !members.insert(id) {
            continue;
        }
        if members.len() > MAX_IMPORT_FLOOR_STATES {
            return Err(Error::Invalid(format!(
                "import floor below {tip_id} exceeds {MAX_IMPORT_FLOOR_STATES} States"
            )));
        }
        let state = store.get_state(&id)?.ok_or_else(|| {
            Error::Invalid(format!(
                "import ancestry absent: converted ancestor {id} of import tip {tip_id} is not installed"
            ))
        })?;
        if state.id() != id {
            return Err(Error::Invalid(format!(
                "import ancestor {id} differs from its stored address"
            )));
        }
        pending.extend(
            state
                .parents
                .iter()
                .copied()
                .filter(|parent| !frontier.contains(parent)),
        );
    }
    tx.execute(
        "INSERT OR IGNORE INTO import_floors(thread,tip,operation) VALUES(?1,?2,?3)",
        params![thread.as_bytes(), tip_id.as_bytes(), operation.as_bytes()],
    )?;
    let mut insert = tx.prepare_cached(
        "INSERT OR IGNORE INTO import_floor_members(thread,tip,member) VALUES(?1,?2,?3)",
    )?;
    for member in &members {
        insert.execute(params![
            thread.as_bytes(),
            tip_id.as_bytes(),
            member.as_bytes()
        ])?;
    }
    Ok(members.len())
}

/// A tip wins over membership: a branch tip inside another branch's floor is
/// still the signed root of its own converted history.
pub(crate) fn role(connection: &Connection, state: &StateId) -> Result<Option<ImportFloorRole>> {
    let tip: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM import_floors WHERE tip=?1)",
        [state.as_bytes()],
        |row| row.get(0),
    )?;
    if tip {
        return Ok(Some(ImportFloorRole::Tip));
    }
    let owner: Option<Vec<u8>> = connection
        .query_row(
            "SELECT tip FROM import_floor_members WHERE member=?1 ORDER BY tip LIMIT 1",
            [state.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    owner
        .map(|bytes| -> Result<ImportFloorRole> {
            let bytes: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invalid("import floor tip identity width".into()))?;
            Ok(ImportFloorRole::Member {
                tip: StateId::from_bytes(bytes),
            })
        })
        .transpose()
}

/// Whether `state` is a recorded converted ancestor in `thread`.
pub(super) fn is_member_in(
    connection: &Connection,
    thread: ContentHash,
    state: &StateId,
) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM import_floor_members WHERE thread=?1 AND member=?2)",
        params![thread.as_bytes(), state.as_bytes()],
        |row| row.get(0),
    )?)
}

/// Recorded import tips of `thread`, for `TransferSelection.exclude_revisions`.
pub(super) fn tips_in(connection: &Connection, thread: ContentHash) -> Result<Vec<StateId>> {
    let mut statement =
        connection.prepare("SELECT tip FROM import_floors WHERE thread=?1 ORDER BY tip")?;
    let rows = statement.query_map([thread.as_bytes()], |row| row.get::<_, Vec<u8>>(0))?;
    let mut tips = Vec::new();
    for row in rows {
        let bytes: [u8; 32] = row?
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invalid("import floor tip identity width".into()))?;
        tips.push(StateId::from_bytes(bytes));
    }
    Ok(tips)
}
