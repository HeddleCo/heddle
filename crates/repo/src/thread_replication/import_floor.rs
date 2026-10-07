//! Durable record of a HYBRID import floor: the converted Git ancestors below
//! one delegated import tip (HeddleCo/heddle#2004, api alpha.42).
//!
//! An import is one signed native operation per branch; its Git ancestors are
//! States in the tip State's parent closure, never operations. Once a Fetch
//! has verified and installed those States, this table remembers which tip
//! they belong to so that
//! - the local visibility walk stops at floor members, as the host's
//!   lineage does, while tips still walk their causal frontier. Walking every
//!   converted commit would hit the bound and withhold the whole checkout;
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
CREATE INDEX IF NOT EXISTS import_floor_members_member ON import_floor_members(member);
CREATE TABLE IF NOT EXISTS import_floor_tiers(
 thread BLOB NOT NULL,tip BLOB NOT NULL,tier INTEGER NOT NULL CHECK(tier IN(1,2)),
 label TEXT NOT NULL,tier_rows INTEGER NOT NULL CHECK(tier_rows>0),
 PRIMARY KEY(thread,tip,tier,label),
 FOREIGN KEY(thread,tip) REFERENCES import_floors(thread,tip) ON DELETE CASCADE);";

/// Hard bound on one floor's membership walk. Double the api contract floor
/// (131072) so boost's 94794 commits fit; a larger import is refused at
/// Prepare, not truncated here.
pub const MAX_IMPORT_FLOOR_STATES: usize = 262_144;

/// How a State relates to recorded import floors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportFloorRole {
    /// The tip of a recorded floor. Its causal frontier must still be walked.
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
                "import ancestry absent: converted ancestor {id} of import tip {tip_id} is not installed; fetch the import tip first"
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

/// Membership is scoped by Thread. A State can be shared by several Threads
/// whose publication floors and disclosure constraints differ.
pub(crate) fn role(
    connection: &Connection,
    thread: ContentHash,
    state: &StateId,
) -> Result<Option<ImportFloorRole>> {
    let owner: Option<Vec<u8>> = connection.query_row(
        "SELECT tip FROM import_floor_members WHERE thread=?1 AND member=?2 ORDER BY tip LIMIT 1",
        params![thread.as_bytes(), state.as_bytes()], |row| row.get(0)).optional()?;
    if let Some(bytes) = owner {
        return Ok(Some(ImportFloorRole::Member {
            tip: StateId::from_bytes(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invalid("import floor tip identity width".into()))?,
            ),
        }));
    }
    let tip: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM import_floors WHERE thread=?1 AND tip=?2)",
        params![thread.as_bytes(), state.as_bytes()],
        |row| row.get(0),
    )?;
    Ok(tip.then_some(ImportFloorRole::Tip))
}

/// Visibility needs Thread context only when that Thread has floor members.
/// Empty floors never stop lineage and need no per-State floor queries.
pub(crate) fn threads_for(connection: &Connection, state: &StateId) -> Result<Vec<ContentHash>> {
    let mut query = connection.prepare(
        "SELECT thread FROM (
        SELECT thread FROM operations WHERE source_revision=?1 AND status=1
        UNION SELECT thread FROM import_floors WHERE tip=?1
        UNION SELECT thread FROM import_floor_members WHERE member=?1) roots
        WHERE EXISTS(SELECT 1 FROM import_floor_members m WHERE m.thread=roots.thread)",
    )?;
    let rows = query.query_map([state.as_bytes()], |row| row.get::<_, Vec<u8>>(0))?;
    rows.map(|row| {
        Ok(ContentHash::from_bytes(
            row?.as_slice()
                .try_into()
                .map_err(|_| Error::Invalid("import floor Thread identity width".into()))?,
        ))
    })
    .collect()
}

pub(crate) fn member_tiers(
    connection: &Connection,
    thread: ContentHash,
    state: &StateId,
) -> Result<Vec<objects::object::VisibilityTier>> {
    let mut query = connection.prepare("SELECT DISTINCT t.tier,t.label FROM import_floor_members m
        JOIN import_floor_tiers t ON t.thread=m.thread AND t.tip=m.tip WHERE m.thread=?1 AND m.member=?2")?;
    let rows = query.query_map(params![thread.as_bytes(), state.as_bytes()], |row| {
        let tier: i32 = row.get(0)?;
        let scope_label = row.get(1)?;
        match tier {
            1 => Ok(objects::object::VisibilityTier::Private { scope_label }),
            2 => Ok(objects::object::VisibilityTier::Restricted { scope_label }),
            other => Err(rusqlite::Error::IntegralValueOutOfRange(
                0,
                i64::from(other),
            )),
        }
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Persist the endpoint's current whole-floor summary in the same installation
/// transaction as its verified ancestry, including on a subsequent PATH Fetch.
pub(super) fn record_tiers_in(
    tx: &Transaction<'_>,
    thread: ContentHash,
    tip: StateId,
    summary: &api::heddle::api::v1alpha2::ImportFloorTierSummary,
) -> Result<()> {
    if !recorded_in(tx, thread, tip)? {
        return Err(Error::Invalid("fetch the import tip first".into()));
    }
    let mut previous = None;
    for row in &summary.rows {
        let key = (row.tier, row.label.as_str());
        if !matches!(row.tier, 1 | 2)
            || row.tier_rows == 0
            || row.tier_rows > i64::MAX as u64
            || previous.is_some_and(|p| p >= key)
        {
            return Err(Error::Invalid("invalid import floor tier summary".into()));
        }
        previous = Some(key);
    }
    tx.execute(
        "DELETE FROM import_floor_tiers WHERE thread=?1 AND tip=?2",
        params![thread.as_bytes(), tip.as_bytes()],
    )?;
    for row in &summary.rows {
        tx.execute("INSERT INTO import_floor_tiers(thread,tip,tier,label,tier_rows) VALUES(?1,?2,?3,?4,?5)",
            params![thread.as_bytes(), tip.as_bytes(), row.tier, row.label, row.tier_rows as i64])?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn floor_roles_are_scoped_by_thread() {
        let connection = Connection::open_in_memory().expect("database");
        initialize_schema(&connection).expect("schema");
        let thread = ContentHash::compute(b"import Thread");
        let other = ContentHash::compute(b"native Thread");
        let tip = StateId::from_bytes([1; 32]);
        let member = StateId::from_bytes([2; 32]);
        connection
            .execute(
                "INSERT INTO import_floors(thread,tip,operation) VALUES(?1,?2,?3)",
                params![thread.as_bytes(), tip.as_bytes(), [3_u8; 32].as_slice()],
            )
            .expect("floor");
        connection
            .execute(
                "INSERT INTO import_floor_members(thread,tip,member) VALUES(?1,?2,?3)",
                params![thread.as_bytes(), tip.as_bytes(), member.as_bytes()],
            )
            .expect("member");
        assert_eq!(
            role(&connection, thread, &tip).expect("role"),
            Some(ImportFloorRole::Tip)
        );
        assert_eq!(
            role(&connection, thread, &member).expect("role"),
            Some(ImportFloorRole::Member { tip })
        );
        assert_eq!(role(&connection, other, &tip).expect("role"), None);
        assert_eq!(role(&connection, other, &member).expect("role"), None);
    }
}
