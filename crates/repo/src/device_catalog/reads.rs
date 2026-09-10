//! Bounded catalog projections, using the same durable identities as mutations.
use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1 as wire;
use prost::Message;
use rusqlite::{OptionalExtension, params};

use super::{
    mutations::mount_in,
    store::{Catalog, Page, SpoolRecord, spool_in},
};
impl Catalog {
    pub fn find_spool_address(&self, address: &str) -> Result<Option<SpoolRecord>> {
        if address.len() > 4096 {
            bail!("Spool address exceeds bound")
        }
        let id: Option<String> = self
            .connection
            .query_row(
                "SELECT id FROM spools WHERE capability_path=?1 AND deleted=0",
                [address],
                |row| row.get(0),
            )
            .optional()?;
        id.map(|id| spool_in(&self.connection, uuid::Uuid::parse_str(&id)?))
            .transpose()
            .map(Option::flatten)
    }
    pub fn children(
        &self,
        parent: uuid::Uuid,
        after: &str,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Page<SpoolRecord>> {
        if limit == 0 || limit > 1024 || max_bytes > 4 * 1024 * 1024 {
            bail!("child page budget")
        }
        let mut statement=self.connection.prepare("SELECT CASE WHEN length(registration)<=16384 THEN registration END,CASE WHEN length(overview)<=262144 THEN overview END,version FROM spools WHERE deleted=0 AND parent=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
        let mut query = statement.query(params![parent.to_string(), after, (limit + 1) as u32])?;
        let mut records = Vec::new();
        let mut bytes = 0usize;
        let mut more = false;
        while let Some(row) = query.next()? {
            let registration: Vec<u8> = row
                .get::<_, Option<Vec<u8>>>(0)?
                .context("child registration bound")?;
            let overview: Vec<u8> = row
                .get::<_, Option<Vec<u8>>>(1)?
                .context("child overview bound")?;
            let size = registration.len() + overview.len();
            if records.len() == limit || bytes + size > max_bytes {
                if records.is_empty() {
                    bail!("child row exceeds byte budget")
                }
                more = true;
                break;
            }
            bytes += size;
            records.push(super::store::decode(&registration, &overview, row.get(2)?)?);
        }
        Ok(Page {
            records,
            has_more: more,
        })
    }
    pub fn bookmarks(
        &self,
        account: &str,
        after: &[u8],
        limit: usize,
        maxbytes: usize,
    ) -> Result<Page<wire::BookmarkRecord>> {
        if limit == 0 || limit > 1024 {
            bail!("bookmark page limit")
        }
        let mut query=self.connection.prepare("SELECT CASE WHEN length(record)<=262144 THEN record ELSE NULL END FROM bookmarks WHERE account=?1 AND target>?2 ORDER BY target LIMIT ?3")?;
        let rows = query.query_map(params![account, after, (limit + 1) as u32], |row| {
            row.get::<_, Option<Vec<u8>>>(0)
        })?;
        let mut records = Vec::new();
        let mut bytes = 0usize;
        let mut more = false;
        for row in rows {
            if records.len() == limit {
                more = true;
                break;
            }
            let row = row?.context("bookmark exceeds record bound")?;
            if bytes + row.len() > maxbytes {
                if records.is_empty() {
                    bail!("bookmark exceeds page budget")
                }
                more = true;
                break;
            }
            bytes += row.len();
            records.push(wire::BookmarkRecord::decode(row.as_slice())?)
        }
        Ok(Page {
            records,
            has_more: more,
        })
    }
    pub fn mounts(
        &self,
        parent: uuid::Uuid,
        after: &str,
        limit: usize,
    ) -> Result<Page<wire::SpoolMount>> {
        if limit == 0 || limit > 1024 {
            bail!("mount page limit")
        }
        let mut query = self.connection.prepare(
            "SELECT id FROM mounts WHERE parent=?1 AND deleted=0 AND id>?2 ORDER BY id LIMIT ?3",
        )?;
        let ids = query
            .query_map(
                params![parent.to_string(), after, (limit + 1) as u32],
                |row| row.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let more = ids.len() > limit;
        let mut records = Vec::new();
        for id in ids.into_iter().take(limit) {
            records.push(
                mount_in(&self.connection, uuid::Uuid::parse_str(&id)?)?
                    .context("mount disappeared inside catalog read")?,
            )
        }
        Ok(Page {
            records,
            has_more: more,
        })
    }
}
impl Catalog {
    /// Retained identity is used only to authorize exact mutation retries and
    /// removal of private bookmarks; it never makes tombstones visible or writable.
    pub fn authorization_path(&self, id: uuid::Uuid) -> Result<Option<String>> {
        let path: Option<String> = self
            .connection
            .query_row(
                "SELECT capability_path FROM spools WHERE id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if path
            .as_ref()
            .is_some_and(|p| p.is_empty() || p.len() > 4096)
        {
            bail!("stored capability path exceeds bound")
        }
        Ok(path)
    }
    pub fn current_mount(&self, id: uuid::Uuid) -> Result<Option<wire::SpoolMount>> {
        mount_in(&self.connection, id)
    }
}
impl Catalog {
    pub fn bookmark(&self, reference: &wire::BookmarkRef) -> Result<wire::BookmarkRecord> {
        let account = &reference.account.as_ref().context("bookmark account")?.id;
        let key = reference.encode_to_vec();
        if key.len() > 4096 {
            bail!("bookmark reference bound")
        }
        let bytes:Option<Vec<u8>>=self.connection.query_row("SELECT CASE WHEN length(record)<=262144 THEN record END FROM bookmarks WHERE account=?1 AND target=?2",params![account,key],|row|row.get(0)).optional()?;
        match bytes {
            Some(bytes) => {
                let record = wire::BookmarkRecord::decode(bytes.as_slice())?;
                if record.r#ref.as_ref() != Some(reference) {
                    bail!("bookmark index integrity")
                };
                Ok(record)
            }
            None => Ok(wire::BookmarkRecord {
                r#ref: Some(reference.clone()),
                ..Default::default()
            }),
        }
    }
}
