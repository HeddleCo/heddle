//! Compare-and-swap catalog edits used inside the caller's receipt transaction.
use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1 as wire;
use prost::Message;
use rusqlite::{OptionalExtension, Transaction, params};

use super::store::{bounded, exact_version, spool_in};

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
    {
        bail!("invalid Spool name")
    }
    Ok(())
}
pub fn validate_settings(settings: &wire::SpoolSettings) -> Result<()> {
    if settings.description.len() > 16 * 1024 {
        bail!("local Spool description is oversized")
    }
    for audience in [settings.audience, settings.default_state_audience] {
        if !matches!(
            wire::Audience::try_from(audience),
            Ok(wire::Audience::Unspecified
                | wire::Audience::Private
                | wire::Audience::Members
                | wire::Audience::Public)
        ) {
            bail!("unknown Spool audience")
        }
    }
    if settings
        .abandoned_thread_retention
        .as_ref()
        .is_some_and(|value| value.seconds < 0 || value.nanos < 0 || value.nanos >= 1_000_000_000)
    {
        bail!("invalid retention duration")
    }
    bounded(settings)
}
pub fn revise(
    tx: &Transaction<'_>,
    id: uuid::Uuid,
    expected: &[u8],
    name: &str,
    settings: &wire::SpoolSettings,
) -> Result<wire::SpoolOverview> {
    validate_name(name)?;
    validate_settings(settings)?;
    let mut record = spool_in(tx, id)?.context("local Spool unavailable")?;
    exact_version(&record.overview.version, expected)?;
    record.overview.name = name.to_owned();
    record.overview.settings = Some(settings.clone());
    record.overview.audience = settings.audience;
    bounded(&record.overview)?;
    tx.execute(
        "UPDATE spools SET overview=?2,version=version+1 WHERE id=?1",
        params![id.to_string(), record.overview.encode_to_vec()],
    )?;
    Ok(spool_in(tx, id)?.context("revised Spool missing")?.overview)
}
pub fn delete(tx: &Transaction<'_>, id: uuid::Uuid, expected: &[u8]) -> Result<()> {
    let record = spool_in(tx, id)?.context("local Spool unavailable")?;
    exact_version(&record.overview.version, expected)?;
    let linked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM spools WHERE parent=?1 AND deleted=0 UNION ALL SELECT 1 FROM mounts WHERE (parent=?1 OR child=?1) AND deleted=0)",[id.to_string()],|row|row.get(0))?;
    if linked {
        bail!("Spool still has children or mounts")
    }
    tx.execute(
        "UPDATE spools SET deleted=1,version=version+1 WHERE id=?1",
        [id.to_string()],
    )?;
    Ok(())
}
pub fn bookmark(
    tx: &Transaction<'_>,
    account: &str,
    request: &wire::SetBookmarkRequest,
) -> Result<wire::BookmarkRecord> {
    let reference = request
        .bookmark
        .as_ref()
        .context("bookmark reference required")?;
    if reference
        .account
        .as_ref()
        .is_none_or(|value| value.id != account)
    {
        bail!("bookmark belongs to another account")
    }
    reference
        .target
        .as_ref()
        .context("bookmark target required")?;
    let target = reference.encode_to_vec();
    if target.len() > 4096 || request.label.len() > 512 {
        bail!("bookmark exceeds bound")
    }
    let prior: Option<i64> = tx
        .query_row(
            "SELECT version FROM bookmarks WHERE account=?1 AND target=?2",
            params![account, target],
            |row| row.get(0),
        )
        .optional()?;
    let actual = prior.map(|v| v.to_be_bytes().to_vec()).unwrap_or_default();
    exact_version(&actual, &request.expected_version)?;
    let version = prior
        .unwrap_or(0)
        .checked_add(1)
        .context("bookmark version exhausted")?;
    let result = wire::BookmarkRecord {
        r#ref: Some(reference.clone()),
        version: version.to_be_bytes().to_vec(),
        label: request.label.clone(),
        bookmarked: request.bookmarked,
    };
    bounded(&result)?;
    tx.execute("INSERT INTO bookmarks(account,target,record,version) VALUES(?1,?2,?3,?4) ON CONFLICT(account,target) DO UPDATE SET record=excluded.record,version=excluded.version",params![account,target,result.encode_to_vec(),version])?;
    Ok(result)
}
pub fn mount(
    tx: &Transaction<'_>,
    request: &wire::SetSpoolMountRequest,
) -> Result<wire::SpoolMount> {
    let mut mount = request.mount.clone().context("mount required")?;
    let reference = mount.r#ref.as_ref().context("mount reference required")?;
    let id = uuid::Uuid::parse_str(&reference.id)?;
    let parent =
        uuid::Uuid::parse_str(&mount.parent.as_ref().context("mount parent required")?.id)?;
    let child = uuid::Uuid::parse_str(&mount.child.as_ref().context("mount child required")?.id)?;
    if reference
        .spool
        .as_ref()
        .is_none_or(|scope| scope.id != parent.to_string())
    {
        bail!("mount reference must bind its parent")
    }
    validate_name(&mount.name)?;
    spool_in(tx, parent)?.context("parent unavailable")?;
    spool_in(tx, child)?.context("child unavailable")?;
    let prior: Option<(i64, bool, String)> = tx
        .query_row(
            "SELECT version,deleted,parent FROM mounts WHERE id=?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if prior
        .as_ref()
        .is_some_and(|(_, _, scope)| scope != &parent.to_string())
    {
        bail!("mount parent is immutable")
    }
    let actual = prior
        .as_ref()
        .map(|(v, _, _)| v.to_be_bytes().to_vec())
        .unwrap_or_default();
    exact_version(&actual, &request.expected_version)?;
    let (count,cycle):(i64,bool)=tx.query_row("WITH RECURSIVE descendants(id) AS (SELECT ?1 UNION SELECT edges.child FROM (SELECT child,parent FROM mounts WHERE deleted=0 AND id<>?3 UNION SELECT id,parent FROM spools WHERE deleted=0 AND parent<>'') edges JOIN descendants d ON edges.parent=d.id LIMIT 1025) SELECT count(*),EXISTS(SELECT 1 FROM descendants WHERE id=?2) FROM descendants",params![child.to_string(),parent.to_string(),id.to_string()],|row|Ok((row.get(0)?,row.get(1)?)))?;
    if count > 1024 {
        bail!("Spool mount graph exceeds traversal bound")
    }
    if cycle {
        bail!("Spool mount would create a cycle")
    }
    let version = prior
        .map(|(v, _, _)| v)
        .unwrap_or(0)
        .checked_add(1)
        .context("mount version exhausted")?;
    tx.execute("INSERT INTO mounts(id,parent,child,name,version) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET child=excluded.child,name=excluded.name,version=excluded.version,deleted=0",params![id.to_string(),parent.to_string(),child.to_string(),mount.name,version])?;
    mount.version = version.to_be_bytes().to_vec();
    Ok(mount)
}
pub fn remove_mount(
    tx: &Transaction<'_>,
    request: &wire::RemoveSpoolMountRequest,
) -> Result<wire::SpoolMount> {
    let requested = request.mount.as_ref().context("mount required")?;
    let reference = requested
        .r#ref
        .as_ref()
        .context("mount reference required")?;
    let id = uuid::Uuid::parse_str(&reference.id)?;
    let mut current = mount_in(tx, id)?.context("mount unavailable")?;
    if current.parent != requested.parent
        || current.child != requested.child
        || current.name != requested.name
        || current.r#ref != requested.r#ref
    {
        bail!("mount changed")
    }
    exact_version(&current.version, &request.expected_version)?;
    tx.execute(
        "UPDATE mounts SET deleted=1,version=version+1 WHERE id=?1",
        [id.to_string()],
    )?;
    let version: i64 = tx.query_row(
        "SELECT version FROM mounts WHERE id=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    current.version = version.to_be_bytes().to_vec();
    Ok(current)
}
pub fn mount_in(
    connection: &rusqlite::Connection,
    id: uuid::Uuid,
) -> Result<Option<wire::SpoolMount>> {
    let row: Option<(String, String, String, i64)> = connection
        .query_row(
            "SELECT parent,child,name,version FROM mounts WHERE id=?1 AND deleted=0",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    Ok(row.map(|(parent, child, name, version)| wire::SpoolMount {
        r#ref: Some(wire::RecordRef {
            id: id.to_string(),
            spool: Some(wire::SpoolRef { id: parent.clone() }),
        }),
        parent: Some(wire::SpoolRef { id: parent }),
        child: Some(wire::SpoolRef { id: child }),
        name,
        version: version.to_be_bytes().to_vec(),
    }))
}
