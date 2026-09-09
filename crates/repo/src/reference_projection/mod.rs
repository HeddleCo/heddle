//! Rebuildable source-reference projections. Signed collaboration and captured
//! source remain authoritative. Callers authorize access and materialize a
//! verified frontier before publishing a root in their metadata transaction.
//!
//! Forking shares an immutable map root. Moving a target copies only its trie
//! route; no referring annotation is rewritten. This is bounded by trie depth,
//! not a claim that identifying all changed source targets takes constant time.
//!
//! Rebuild inputs are the admitted collaboration heads (typed properties and
//! original source references), the selected capture's immutable source-target
//! map/root and its referenced objects. `MapStore` caches the existing canonical
//! map node encoding; it does not replace the object store. Capture publication
//! and replication adapters must install those canonical objects first. This
//! module neither authors nor replicates a new map-root field on their behalf.
use objects::object::{
    AnnotationTag, AnnotationValue, CollaborationScope, ContentHash,
    source_target::{SourceTargetBinding, SourceTargetReference},
    source_target_map::{MAX_NODE_BYTES, MapBudget, SourceTargetMap, SourceTargetMapStore},
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reference projection database: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("invalid reference projection: {0}")]
    Invalid(&'static str),
    #[error("source reference map: {0}")]
    Map(String),
    #[error("reference projection changed; retry against its current root")]
    Stale,
}

pub(crate) fn initialize_schema(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS reference_map_nodes (
            hash BLOB PRIMARY KEY CHECK(length(hash)=32), body BLOB NOT NULL
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS reference_roots (
            spool BLOB NOT NULL CHECK(length(spool)=16),
            thread BLOB NOT NULL CHECK(length(thread)=32),
            root BLOB CHECK(root IS NULL OR length(root)=32),
            PRIMARY KEY(spool,thread)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS annotation_property_index (
            spool BLOB NOT NULL CHECK(length(spool)=16),
            thread BLOB NOT NULL CHECK(length(thread)=32),
            record BLOB NOT NULL CHECK(length(record)=32),
            name TEXT NOT NULL, kind INTEGER NOT NULL CHECK(kind BETWEEN 0 AND 3),
            value BLOB NOT NULL,
            PRIMARY KEY(spool,thread,record,name)
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS annotation_property_lookup
            ON annotation_property_index(spool,thread,name,kind,value,record);",
    )
}

fn thread(scope: &CollaborationScope) -> Result<ContentHash, Error> {
    if scope.spool.is_nil() {
        return Err(Error::Invalid("Spool must not be nil"));
    }
    scope
        .thread
        .ok_or(Error::Invalid("concrete Thread required"))
}
fn hash(bytes: Vec<u8>) -> Result<ContentHash, Error> {
    Ok(ContentHash::from_bytes(
        bytes
            .try_into()
            .map_err(|_| Error::Invalid("invalid stored hash"))?,
    ))
}

/// None means an unmaterialized Thread; Some(None) is a materialized empty map.
pub fn root(
    db: &Connection,
    scope: &CollaborationScope,
) -> Result<Option<Option<ContentHash>>, Error> {
    let id = thread(scope)?;
    let value: Option<Option<Vec<u8>>> = db
        .query_row(
            "SELECT root FROM reference_roots WHERE spool=?1 AND thread=?2",
            params![scope.spool.as_bytes(), id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    value.map(|value| value.map(hash).transpose()).transpose()
}

/// Install a verified projection root with exact compare-and-swap semantics.
/// The transaction also contains the capture/frontier publication. No root is
/// installed if map construction fails or that transaction rolls back.
pub fn publish_root(
    tx: &Transaction<'_>,
    scope: &CollaborationScope,
    expected: Option<Option<ContentHash>>,
    next: Option<ContentHash>,
) -> Result<(), Error> {
    let id = thread(scope)?;
    if root(tx, scope)? != expected {
        return Err(Error::Stale);
    }
    if let Some(next) = next {
        let present: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM reference_map_nodes WHERE hash=?1)",
            [next.as_bytes()],
            |row| row.get(0),
        )?;
        if !present {
            return Err(Error::Invalid("root node is missing"));
        }
    }
    tx.execute(
        "INSERT INTO reference_roots(spool,thread,root) VALUES(?1,?2,?3)
        ON CONFLICT(spool,thread) DO UPDATE SET root=excluded.root",
        params![
            scope.spool.as_bytes(),
            id.as_bytes(),
            next.map(|v| v.as_bytes().to_vec())
        ],
    )?;
    Ok(())
}

/// The caller verifies actual Thread inheritance. Cross-Spool inheritance is
/// deliberately unavailable through this cache helper.
pub fn fork(
    tx: &Transaction<'_>,
    parent: &CollaborationScope,
    child: &CollaborationScope,
) -> Result<(), Error> {
    if parent.spool != child.spool {
        return Err(Error::Invalid("fork crosses Spool"));
    }
    let inherited = root(tx, parent)?.ok_or(Error::Invalid("parent is not materialized"))?;
    publish_root(tx, child, None, inherited)
}

pub struct MapStore<'a> {
    db: &'a Connection,
}
impl<'a> MapStore<'a> {
    pub fn new(db: &'a Connection) -> Self {
        Self { db }
    }
}
impl SourceTargetMapStore for MapStore<'_> {
    type Error = Error;
    fn read(&mut self, hash: ContentHash, max_bytes: usize) -> Result<Option<Vec<u8>>, Error> {
        // SQLite checks the length before returning/allocating the blob body.
        let result: Option<(i64, Option<Vec<u8>>)> = self.db.query_row(
            "SELECT length(body), CASE WHEN length(body)<=?2 THEN body END FROM reference_map_nodes WHERE hash=?1",
            params![hash.as_bytes(), i64::try_from(max_bytes).map_err(|_| Error::Invalid("invalid read budget"))?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        match result {
            None => Ok(None),
            Some((_, Some(body))) => Ok(Some(body)),
            Some(_) => Err(Error::Invalid("map node exceeds read budget")),
        }
    }
    fn write(&mut self, hash: ContentHash, body: Vec<u8>) -> Result<(), Error> {
        if body.len() > MAX_NODE_BYTES || ContentHash::compute_typed("blob", &body) != hash {
            return Err(Error::Invalid("invalid immutable map node"));
        }
        self.db.execute(
            "INSERT INTO reference_map_nodes(hash,body) VALUES(?1,?2) ON CONFLICT(hash) DO NOTHING",
            params![hash.as_bytes(), body],
        )?;
        Ok(())
    }
}

/// Resolve an inherited or explicitly named reference against its selected
/// Thread. Pinned revisions require a verified historical root supplied by the
/// caller through `resolve_at`; they never silently follow the current head.
pub fn resolve(
    db: &Connection,
    viewed: &CollaborationScope,
    reference: &SourceTargetReference,
    budget: &mut MapBudget,
) -> Result<Option<ContentHash>, Error> {
    if matches!(
        reference.binding,
        SourceTargetBinding::PinnedRevision { .. }
    ) {
        return Err(Error::Invalid("pinned reference requires historical root"));
    }
    let scope = reference
        .binding
        .scope(viewed)
        .map_err(|_| Error::Invalid("invalid binding scope"))?;
    let selected = root(db, scope)?.ok_or(Error::Invalid("Thread is not materialized"))?;
    resolve_at(db, selected, reference.target, budget)
}
pub fn resolve_at(
    db: &Connection,
    root: Option<ContentHash>,
    target: ContentHash,
    budget: &mut MapBudget,
) -> Result<Option<ContentHash>, Error> {
    SourceTargetMap::get(&mut MapStore::new(db), root, target, budget)
        .map_err(|error| Error::Map(error.to_string()))
}

// Sort keys preserve the canonical model's type separation, including exact
// decimal ordering without an f64 conversion or SQLite integer overflow.
fn value_key(value: &AnnotationValue) -> Result<(i64, Vec<u8>), Error> {
    value
        .validate()
        .map_err(|_| Error::Invalid("invalid annotation value"))?;
    Ok(match value {
        AnnotationValue::Text(value) => (0, value.as_bytes().to_vec()),
        AnnotationValue::Boolean(value) => (1, vec![u8::from(*value)]),
        AnnotationValue::Integer(value) => {
            (2, ((*value as u64) ^ (1 << 63)).to_be_bytes().to_vec())
        }
        AnnotationValue::Decimal(value) => {
            let scaled = i128::from(value.coefficient) * 10_i128.pow(9 - value.scale);
            (3, ((scaled as u128) ^ (1 << 127)).to_be_bytes().to_vec())
        }
    })
}

/// Index one property from a verified materialized record. This is not an
/// authoring endpoint. Record IDs identify exact materialized heads; callers
/// delete superseded rows in the same transaction that installs their heads.
pub fn put_property(
    tx: &Transaction<'_>,
    scope: &CollaborationScope,
    record: ContentHash,
    name: &str,
    value: &AnnotationValue,
) -> Result<(), Error> {
    let id = thread(scope)?;
    AnnotationTag::Property {
        key: name.into(),
        value: value.clone(),
    }
    .validate()
    .map_err(|_| Error::Invalid("invalid annotation property"))?;
    let (kind, value) = value_key(value)?;
    tx.execute("INSERT INTO annotation_property_index(spool,thread,record,name,kind,value) VALUES(?1,?2,?3,?4,?5,?6)
        ON CONFLICT(spool,thread,record,name) DO UPDATE SET kind=excluded.kind,value=excluded.value",
        params![scope.spool.as_bytes(), id.as_bytes(), record.as_bytes(), name, kind, value])?;
    Ok(())
}

/// Replace the property projection of one materialized head. Validate the full
/// canonical tag set before deleting anything; duplicate keys cannot silently
/// overwrite one another. A deleted/superseded head supplies an empty tag list.
pub fn project_properties(
    tx: &Transaction<'_>,
    scope: &CollaborationScope,
    record: ContentHash,
    tags: &[AnnotationTag],
) -> Result<(), Error> {
    objects::object::validate_annotation_tags(tags)
        .map_err(|_| Error::Invalid("invalid canonical annotation tags"))?;
    let id = thread(scope)?;
    tx.execute(
        "DELETE FROM annotation_property_index WHERE spool=?1 AND thread=?2 AND record=?3",
        params![scope.spool.as_bytes(), id.as_bytes(), record.as_bytes()],
    )?;
    for tag in tags {
        if let AnnotationTag::Property { key, value } = tag {
            put_property(tx, scope, record, key, value)?;
        }
    }
    Ok(())
}

/// Indexed typed equality with deterministic keyset pagination. Returned IDs
/// are candidates: callers still apply current visibility and head admission.
pub fn property_equal(
    db: &Connection,
    scope: &CollaborationScope,
    name: &str,
    value: &AnnotationValue,
    after: Option<ContentHash>,
    limit: u32,
) -> Result<Vec<ContentHash>, Error> {
    if limit == 0 || limit > 1024 {
        return Err(Error::Invalid("invalid query limit"));
    }
    let id = thread(scope)?;
    let (kind, value) = value_key(value)?;
    let mut statement = db.prepare("SELECT record FROM annotation_property_index
        WHERE spool=?1 AND thread=?2 AND name=?3 AND kind=?4 AND value=?5 AND record>?6 ORDER BY record LIMIT ?7")?;
    let rows = statement.query_map(
        params![
            scope.spool.as_bytes(),
            id.as_bytes(),
            name,
            kind,
            value,
            after.map(|v| v.as_bytes().to_vec()).unwrap_or_default(),
            limit
        ],
        |row| row.get::<_, Vec<u8>>(0),
    )?;
    rows.map(|row| hash(row?)).collect()
}

#[cfg(test)]
mod tests;
