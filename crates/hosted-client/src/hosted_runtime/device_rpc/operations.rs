//! Exact caller-scoped command receipts over the committed metadata change feed.
use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use api::heddle::api::v2alpha1::*;
use objects::object::{ContentHash, OperationId};
use prost::Message;
use repo::operation_dedup::observation;

use super::{
    DeviceRpc,
    account_auth::AccountSession,
    account_observe::{decode_page, encode_page, page_size},
};

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Cursor {
    spool: usize,
    record: Option<ContentHash>,
}
struct Scope {
    spool: repo::device_catalog::DeviceSpool,
    namespace: String,
    records: Vec<ContentHash>,
}

impl super::stream::Event for OperationEvent {
    fn frame(&mut self, frame: StreamFrame) {
        self.frame = Some(frame);
    }
}

impl DeviceRpc {
    pub(super) async fn observe_operations(
        &self,
        session: &AccountSession,
        body: &[u8],
        send: iroh::endpoint::SendStream,
    ) -> Result<()> {
        let request = ObserveOperationsRequest::decode(body)?;
        ensure!(
            request.client_operation_ids.len() <= 128
                && request.operations.len() <= 128
                && request.spools.len() <= 32,
            "operation selector bound"
        );
        let ids = request
            .client_operation_ids
            .iter()
            .map(|id| id.parse::<OperationId>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut selected = BTreeSet::new();
        for spool in &request.spools {
            selected.insert(uuid::Uuid::parse_str(&spool.id)?);
        }
        for record in &request.operations {
            selected.insert(uuid::Uuid::parse_str(
                &record
                    .spool
                    .as_ref()
                    .context("operation Spool required")?
                    .id,
            )?);
        }
        ensure!(
            !selected.is_empty() && selected.len() <= 32,
            "operation observation requires explicit bounded Spools"
        );
        let mut scopes = Vec::new();
        for id in selected {
            let spool = repo::device_catalog::load(&self.home, id)?;
            let facts = session.facts(Some(&spool.capability_path))?;
            let namespace =
                serde_json::to_string(&(session.principal.as_str(), &facts.delegation_agent_id))?;
            let records = request
                .operations
                .iter()
                .filter(|record| {
                    record
                        .spool
                        .as_ref()
                        .is_some_and(|scope| scope.id == id.to_string())
                })
                .map(|record| ContentHash::from_hex(&record.id))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            scopes.push(Scope {
                spool,
                namespace,
                records,
            });
        }
        let feed = self.account_feed()?;
        feed.refresh()?;
        let mut normalized = request.clone();
        normalized.observe = None;
        normalized.page = None;
        let authority = Authority {
            session,
            scopes: &scopes,
        };
        self.observe_authorized_view(
            &authority,
            "/heddle.api.v2alpha1.OperationService/ObserveOperations",
            &normalized.encode_to_vec(),
            request.observe.clone().unwrap_or_default(),
            send,
            feed.changes.subscribe(),
            |budget, binding| {
                let before = operation_version(&scopes)?;
                let value = self.operation_snapshot(
                    session,
                    &scopes,
                    &ids,
                    &request.page.clone().unwrap_or_default(),
                    budget,
                    binding,
                )?;
                if operation_version(&scopes)? != before {
                    return Err(super::stream::SnapshotChanged.into());
                }
                Ok((value.0, value.1, before))
            },
            || operation_version(&scopes),
        )
        .await
    }

    fn operation_snapshot(
        &self,
        session: &AccountSession,
        scopes: &[Scope],
        ids: &[OperationId],
        page: &PageRequest,
        budget: &ReadBudget,
        binding: &[u8],
    ) -> Result<(Vec<(String, OperationEvent)>, PageInfo)> {
        let mut cursor: Cursor =
            decode_page(&page.after_page, binding, b"operations")?.unwrap_or_default();
        ensure!(cursor.spool <= scopes.len(), "operation page scope bound");
        let limit = page_size(page, budget).min(256);
        let mut events = Vec::new();
        while cursor.spool < scopes.len() && events.len() < limit {
            let scope = &scopes[cursor.spool];
            session.facts(Some(&scope.spool.capability_path))?;
            let remaining = limit - events.len();
            let mut rows = observation::page(
                &scope.spool.heddle_dir,
                &scope.namespace,
                ids,
                &scope.records,
                cursor.record,
                remaining + 1,
            )?;
            let exhausted = rows.len() <= remaining;
            rows.truncate(remaining);
            for row in rows {
                cursor.record = Some(row.record);
                let record = OperationRecord {
                    r#ref: Some(RecordRef {
                        spool: Some(SpoolRef {
                            id: scope.spool.id.to_string(),
                        }),
                        id: row.record.to_string(),
                    }),
                    client_operation_id: row.operation_id.to_string(),
                    version: row.version().as_bytes().to_vec(),
                    // A reservation is not evidence that an executor started.
                    state: if row.pending {
                        operation_record::State::Unspecified
                    } else {
                        operation_record::State::Completed
                    } as i32,
                    unit: row.method,
                    cancellation_supported: false,
                    ..Default::default()
                };
                events.push((
                    row.record.to_string(),
                    OperationEvent {
                        frame: None,
                        payload: Some(operation_event::Payload::Operation(record)),
                    },
                ));
            }
            if exhausted {
                cursor.spool += 1;
                cursor.record = None;
            } else {
                break;
            }
        }
        let exhausted = cursor.spool == scopes.len();
        let next_page = if exhausted {
            Vec::new()
        } else {
            encode_page(&cursor, binding, b"operations")?
        };
        Ok((
            events,
            PageInfo {
                exhausted,
                next_page,
                ..Default::default()
            },
        ))
    }
}
fn operation_version(scopes: &[Scope]) -> Result<Vec<u8>> {
    let mut hash = blake3::Hasher::new_derive_key("heddle-device-operation-view-v2");
    for scope in scopes {
        hash.update(scope.spool.id.as_bytes());
        hash.update(&observation::generation(&scope.spool.heddle_dir)?);
    }
    Ok(hash.finalize().as_bytes().to_vec())
}

struct Authority<'a> {
    session: &'a AccountSession,
    scopes: &'a [Scope],
}
impl super::stream::ObservationAuthority for Authority<'_> {
    fn binding(&self) -> Vec<u8> {
        self.session.binding()
    }
    fn expires(&self) -> i64 {
        self.session.expires
    }
    fn check_clock(&self) -> Result<()> {
        self.session.check_clock()?;
        for scope in self.scopes {
            self.session.facts(Some(&scope.spool.capability_path))?;
        }
        Ok(())
    }
    fn check_current(&self, home: &std::path::Path) -> Result<()> {
        self.session.check_current(home)?;
        for scope in self.scopes {
            let current = repo::device_catalog::load(home, scope.spool.id)?;
            ensure!(
                current.heddle_dir == scope.spool.heddle_dir
                    && current.root == scope.spool.root
                    && current.capability_path == scope.spool.capability_path,
                "operation Spool registration changed"
            );
        }
        self.check_clock()
    }
}
