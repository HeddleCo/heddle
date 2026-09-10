//! One bounded merge over local per-Spool list indexes; exact views remain native.
use anyhow::{Context, Result, ensure};
use api::heddle::api::v2alpha1::*;
use repo::{
    device_catalog::DeviceSpool,
    thread_replication::{ThreadReplica, listing},
};

use super::{
    DeviceRpc,
    account_auth::AccountSession,
    account_observe::{decode_page, encode_page, page_size},
};
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct Cursor {
    name: String,
    updated: i64,
    thread: Vec<u8>,
}
impl Cursor {
    fn from_row(row: &listing::Row) -> Self {
        Self {
            name: row.name.clone(),
            updated: row.updated,
            thread: row.thread.as_bytes().to_vec(),
        }
    }
}
impl DeviceRpc {
    pub(super) fn local_threads(
        &self,
        session: &AccountSession,
        query: &ThreadQuery,
        page: &PageRequest,
        budget: &ReadBudget,
        binding: &[u8],
    ) -> Result<(Vec<ThreadOverview>, PageInfo)> {
        ensure!(query.text.len() <= 4096, "Thread text filter bound");
        ensure!(
            query.attention_for_principal.is_empty() && query.participating_principal.is_empty(),
            "local attention/participation query is unavailable until its facet index is present"
        );
        for lifecycle in &query.lifecycle {
            ThreadLifecycle::try_from(*lifecycle).context("unknown lifecycle")?;
        }
        for readiness in &query.readiness {
            ReviewReadiness::try_from(*readiness).context("unknown readiness")?;
        }
        let order = thread_query::Order::try_from(query.order).context("unknown Thread order")?;
        let by_name = order == thread_query::Order::NameAsc;
        let cursor: Option<Cursor> = decode_page(&page.after_page, binding, b"threads")?;
        let after = cursor.as_ref().map(|c| listing::Cursor {
            name: c.name.clone(),
            updated: c.updated,
            thread: c.thread.clone(),
        });
        let spools = self.thread_spools(session, query)?;
        let size = page_size(page, budget);
        let per_spool = (4096 / spools.len().max(1)).min(size + 1).min(1023).max(1);
        let compare = |a: &listing::Row, b: &listing::Row| {
            if by_name {
                a.name.cmp(&b.name).then(a.thread.cmp(&b.thread))
            } else {
                b.updated.cmp(&a.updated).then(b.thread.cmp(&a.thread))
            }
        };
        let mut candidates = Vec::new();
        let mut cutoffs = Vec::new();
        let mut bytes = 0usize;
        for spool in &spools {
            let mut rows =
                listing::page(&spool.heddle_dir, by_name, after.as_ref(), per_spool + 1)?;
            let more = rows.len() > per_spool;
            rows.truncate(per_spool);
            if more {
                cutoffs.push(rows.last().context("bounded list cutoff")?.clone())
            }
            for row in rows {
                bytes = bytes.saturating_add(row.name.len() + row.intent.len());
                ensure!(
                    bytes <= 4 * 1024 * 1024,
                    "Thread candidate window exceeds aggregate byte budget"
                );
                candidates.push((spool, row));
            }
        }
        candidates.sort_by(|a, b| compare(&a.1, &b.1));
        cutoffs.sort_by(compare);
        let cutoff = cutoffs.first();
        let mut output = Vec::new();
        let mut scanned = None;
        let mut more = !cutoffs.is_empty();
        for (index, (spool, row)) in candidates.iter().enumerate() {
            if cutoff.is_some_and(|c| compare(row, c).is_gt()) {
                more = true;
                break;
            }
            scanned = Some(Cursor::from_row(row));
            let matches = (query.lifecycle.is_empty() || query.lifecycle.contains(&row.lifecycle))
                && (query.readiness.is_empty()
                    || query.readiness.contains(&(ReviewReadiness::Unknown as i32)))
                && (query.text.is_empty()
                    || row.name.contains(&query.text)
                    || row.intent.contains(&query.text))
                && query.parent.as_ref().is_none_or(|p| {
                    p.spool
                        .as_ref()
                        .is_some_and(|p| p.id == spool.id.to_string())
                        && p.id
                            .as_ref()
                            .is_some_and(|p| row.parent.is_some_and(|id| p.value == id.as_bytes()))
                });
            if matches {
                let facts = session.facts(Some(&spool.capability_path))?;
                let replica = ThreadReplica::open(&spool.heddle_dir, row.thread)?;
                let repository = repo::Repository::open(&spool.root)?;
                if !super::auth::thread_visible(
                    &repository,
                    &replica,
                    uuid::Uuid::parse_str(&session.principal)?,
                    facts.delegation_agent_id.as_deref(),
                )? {
                    continue;
                }
                let mut overview = self.thread_overview_for_spool(spool, &replica, |method| {
                    session.permits_method(method, &spool.capability_path)
                })?;
                if overview.name != row.name || overview.lifecycle != row.lifecycle {
                    return Err(super::stream::SnapshotChanged.into());
                }
                overview.updated_at =
                    chrono::DateTime::from_timestamp_millis(row.updated).map(|time| {
                        prost_types::Timestamp {
                            seconds: time.timestamp(),
                            nanos: time.timestamp_subsec_nanos() as i32,
                        }
                    });
                output.push(overview);
                if output.len() == size {
                    more |= index + 1 < candidates.len();
                    break;
                }
            }
        }
        let next = if more {
            encode_page(
                &scanned.context("Thread page failed to make progress")?,
                binding,
                b"threads",
            )?
        } else {
            Vec::new()
        };
        Ok((
            output,
            PageInfo {
                next_page: next,
                exhausted: !more,
                matching_count: None,
            },
        ))
    }
    pub(super) fn thread_spools(
        &self,
        session: &AccountSession,
        query: &ThreadQuery,
    ) -> Result<Vec<DeviceSpool>> {
        ensure!(
            query.spools.len() <= 64,
            "select at most 64 local Spools per Thread view"
        );
        let Some(catalog) = repo::device_catalog::store::Catalog::read(&self.home)? else {
            return Ok(Vec::new());
        };
        let mut result = Vec::new();
        for spool in catalog.registrations()? {
            if !query.spools.is_empty()
                && !query.spools.iter().any(|r| r.id == spool.id.to_string())
            {
                continue;
            }
            if session.permits(&spool.capability_path) {
                result.push(spool);
            }
        }
        ensure!(
            result.len() <= 64,
            "account has more than 64 Spools; select a bounded Thread scope"
        );
        Ok(result)
    }
}
