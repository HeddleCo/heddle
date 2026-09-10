//! Bounded receipt metadata excludes response bytes and unnamespaced CLI records.
use objects::object::{ContentHash, OperationId};
use prost::Message;
use rusqlite::{params_from_iter, types::Value};

use super::{HeddleError, Result, database_error};
#[derive(Clone, Debug)]
pub struct Receipt {
    pub record: ContentHash,
    pub operation_id: OperationId,
    pub method: String,
    pub request_hash: [u8; 32],
    pub created_at: i64,
    pub pending: bool,
    pub execution: Option<api::heddle::api::v2alpha1::OperationRecord>,
    pub executor: Option<String>,
}
impl Receipt {
    pub fn version(&self) -> ContentHash {
        let mut hash = blake3::Hasher::new_derive_key("heddle-device-operation-version-v2");
        hash.update(self.record.as_bytes());
        hash.update(&self.request_hash);
        hash.update(&self.created_at.to_be_bytes());
        hash.update(&[u8::from(self.pending)]);
        hash.update(self.method.as_bytes());
        ContentHash::from_bytes(*hash.finalize().as_bytes())
    }
}
pub fn page(
    directory: &std::path::Path,
    namespace: &str,
    ids: &[OperationId],
    records: &[ContentHash],
    after: Option<ContentHash>,
    limit: usize,
) -> Result<Vec<Receipt>> {
    if namespace.is_empty()
        || namespace.len() > 1024
        || ids.len() > 128
        || records.len() > 128
        || !(1..=257).contains(&limit)
    {
        return Err(HeddleError::InvalidObject(
            "invalid scoped operation page bound".into(),
        ));
    }
    let connection = crate::local_metadata::open_existing(directory)
        .map_err(database_error)?
        .ok_or_else(|| database_error("local metadata missing"))?;
    let mut sql = String::from(
        "SELECT r.record_id,r.operation_id,r.verb,r.request_hash,r.created_at,r.pending,e.record,e.executor FROM operation_receipts r LEFT JOIN device_operations e ON e.namespace=r.namespace AND e.record_id=r.record_id WHERE r.namespace=?1 AND r.record_id>?2",
    );
    let mut values = vec![
        Value::Text(namespace.into()),
        Value::Blob(after.map(|id| id.as_bytes().to_vec()).unwrap_or_default()),
    ];
    if !ids.is_empty() || !records.is_empty() {
        sql.push_str(" AND (");
        if !ids.is_empty() {
            sql.push_str("r.operation_id IN (");
            for (index, id) in ids.iter().enumerate() {
                if index > 0 {
                    sql.push(',');
                }
                sql.push('?');
                values.push(Value::Text(id.to_string()));
            }
            sql.push(')');
        }
        if !records.is_empty() {
            if !ids.is_empty() {
                sql.push_str(" OR ");
            }
            sql.push_str("r.record_id IN (");
            for (index, id) in records.iter().enumerate() {
                if index > 0 {
                    sql.push(',');
                }
                sql.push('?');
                values.push(Value::Blob(id.as_bytes().to_vec()));
            }
            sql.push(')');
        }
        sql.push(')');
    }
    sql.push_str(" ORDER BY r.record_id LIMIT ?");
    values.push(Value::Integer(limit as i64));
    let mut query = connection.prepare(&sql).map_err(database_error)?;
    let rows = query
        .query_map(params_from_iter(values), |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get::<_, Option<Vec<u8>>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })
        .map_err(database_error)?;
    rows.map(|row| {
        let (record, id, method, hash, created_at, pending, execution, executor) =
            row.map_err(database_error)?;
        Ok(Receipt {
            record: ContentHash::from_bytes(
                record
                    .try_into()
                    .map_err(|_| database_error("invalid receipt record key"))?,
            ),
            operation_id: id.parse().map_err(database_error)?,
            method,
            request_hash: hash
                .try_into()
                .map_err(|_| database_error("invalid receipt hash"))?,
            created_at,
            pending,
            executor,
            execution: execution
                .map(|bytes| {
                    api::heddle::api::v2alpha1::OperationRecord::decode(bytes.as_slice())
                        .map_err(database_error)
                })
                .transpose()?,
        })
    })
    .collect()
}

pub fn generation(directory: &std::path::Path) -> Result<Vec<u8>> {
    let connection = crate::local_metadata::open_existing(directory)
        .map_err(database_error)?
        .ok_or_else(|| database_error("local metadata missing"))?;
    let cursor: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(cursor),0) FROM metadata_changes",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    Ok(cursor.to_be_bytes().to_vec())
}
