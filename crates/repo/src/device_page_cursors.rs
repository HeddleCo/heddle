//! Private, retryable continuation state for device read projections.
//! A public page token is random; it never serializes an unserved record ID.
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::local_metadata;

const TTL_SECONDS: i64 = 24 * 60 * 60;
const MAX_PER_SCOPE: i64 = 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("local page cursor store: {0}")]
    Store(#[from] local_metadata::Error),
    #[error("local page cursor SQL: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("page cursor expired or belongs to another view; restart the observation")]
    Expired,
    #[error("page cursor clock is before the Unix epoch")]
    Clock,
    #[error("invalid page cursor scope or record ID")]
    Invalid,
}

pub fn initialize_schema(connection: &rusqlite::Connection) -> Result<(), Error> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS device_page_cursors(
        token BLOB PRIMARY KEY CHECK(length(token)=32),
        scope BLOB NOT NULL CHECK(length(scope)=32),
        binding BLOB NOT NULL CHECK(length(binding)<=128),
        section TEXT NOT NULL CHECK(length(section)<=32),
        last_scanned BLOB NOT NULL CHECK(length(last_scanned)=32),
        issued_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS device_page_cursors_scope_age
      ON device_page_cursors(scope, issued_at, token);
    CREATE INDEX IF NOT EXISTS device_page_cursors_expiry
      ON device_page_cursors(expires_at);",
    )?;
    Ok(())
}

fn now() -> Result<i64, Error> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Clock)?
        .as_secs() as i64)
}

/// Issue a stable private lookup for a scan boundary. Old tokens remain valid
/// until their TTL or the per-actor retention cap, so retries and multiple
/// tabs can repeat the same page while its token remains retained.
pub fn issue(
    heddle_dir: &Path,
    scope: [u8; 32],
    binding: &[u8],
    section: &str,
    last_scanned: [u8; 32],
) -> Result<[u8; 32], Error> {
    issue_with_limit(
        heddle_dir,
        scope,
        binding,
        section,
        last_scanned,
        MAX_PER_SCOPE,
    )
}

fn issue_with_limit(
    heddle_dir: &Path,
    scope: [u8; 32],
    binding: &[u8],
    section: &str,
    last_scanned: [u8; 32],
    max_per_scope: i64,
) -> Result<[u8; 32], Error> {
    if binding.len() > 128 || section.len() > 32 || section.is_empty() {
        return Err(Error::Invalid);
    }
    let time = now()?;
    let token: [u8; 32] = rand::random();
    let mut connection = local_metadata::open(heddle_dir)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "DELETE FROM device_page_cursors WHERE expires_at<=?1",
        [time],
    )?;
    transaction.execute("INSERT INTO device_page_cursors(token,scope,binding,section,last_scanned,issued_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![token.as_slice(), scope.as_slice(), binding, section, last_scanned.as_slice(), time, time + TTL_SECONDS])?;
    transaction.execute(
        "DELETE FROM device_page_cursors WHERE token IN (
        SELECT token FROM device_page_cursors WHERE scope=?1
        ORDER BY rowid DESC LIMIT -1 OFFSET ?2
    )",
        params![scope.as_slice(), max_per_scope],
    )?;
    transaction.commit()?;
    Ok(token)
}

/// Resolve a random token after the caller has authenticated. Authorization of
/// the Thread and every resumed row remains the read handler's responsibility.
pub fn resume(
    heddle_dir: &Path,
    scope: [u8; 32],
    binding: &[u8],
    section: &str,
    token: &[u8],
) -> Result<[u8; 32], Error> {
    resume_at(heddle_dir, scope, binding, section, token, now()?)
}

/// Resolve a Search cursor whose private section carries the selected Spool
/// ordinal. The public token remains random and reveals neither ordinal nor
/// the last served operation identity.
pub fn resume_search(
    heddle_dir: &Path,
    scope: [u8; 32],
    binding: &[u8],
    token: &[u8],
) -> Result<(usize, [u8; 32]), Error> {
    if token.len() != 32 || binding.len() > 128 {
        return Err(Error::Invalid);
    }
    let connection = local_metadata::open(heddle_dir)?;
    let stored: Option<(String, Vec<u8>)> = connection
        .query_row(
            "SELECT section,last_scanned FROM device_page_cursors WHERE token=?1 AND scope=?2 AND binding=?3 AND section LIKE 'search:%' AND expires_at>?4",
            params![token, scope.as_slice(), binding, now()?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (section, operation) = stored.ok_or(Error::Expired)?;
    let ordinal = section
        .strip_prefix("search:")
        .ok_or(Error::Invalid)?
        .parse::<usize>()
        .map_err(|_| Error::Invalid)?;
    let operation = operation.try_into().map_err(|_| Error::Invalid)?;
    Ok((ordinal, operation))
}

fn resume_at(
    heddle_dir: &Path,
    scope: [u8; 32],
    binding: &[u8],
    section: &str,
    token: &[u8],
    time: i64,
) -> Result<[u8; 32], Error> {
    if token.len() != 32 || binding.len() > 128 || section.len() > 32 {
        return Err(Error::Invalid);
    }
    let connection = local_metadata::open(heddle_dir)?;
    let stored: Option<Vec<u8>> = connection.query_row(
        "SELECT last_scanned FROM device_page_cursors WHERE token=?1 AND scope=?2 AND binding=?3 AND section=?4 AND expires_at>?5",
        params![token, scope.as_slice(), binding, section, time],
        |row| row.get(0),
    ).optional()?;
    stored
        .ok_or(Error::Expired)?
        .try_into()
        .map_err(|_| Error::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_cursor_keeps_spool_and_operation_private_and_retries() {
        let home = tempfile::tempdir().expect("local search cursor store");
        let scope = [19; 32];
        let operation = [29; 32];
        let token = issue(home.path(), scope, b"search-query", "search:3", operation)
            .expect("issue private search position");
        assert_ne!(token, operation);
        assert_eq!(
            resume_search(home.path(), scope, b"search-query", &token).expect("first page"),
            (3, operation)
        );
        assert_eq!(
            resume_search(home.path(), scope, b"search-query", &token).expect("retry"),
            (3, operation)
        );
        assert!(matches!(
            resume_search(home.path(), [20; 32], b"search-query", &token),
            Err(Error::Expired)
        ));
        assert!(matches!(
            resume_search(home.path(), scope, b"other-query", &token),
            Err(Error::Expired)
        ));
    }

    #[test]
    fn opaque_cursor_retries_and_rejects_other_reader_or_query() {
        let home = tempfile::tempdir().expect("private local metadata");
        let scope = [7; 32];
        let hidden = [8; 32];
        let token = issue(home.path(), scope, b"query-one", "captures", hidden)
            .expect("issued continuation");
        assert_ne!(token, hidden, "public token does not disclose hidden ID");
        assert_eq!(
            resume(home.path(), scope, b"query-one", "captures", &token).expect("first resume"),
            hidden
        );
        assert_eq!(
            resume(home.path(), scope, b"query-one", "captures", &token).expect("retry"),
            hidden
        );
        for (reader, binding, section) in [
            ([9; 32], b"query-one".as_slice(), "captures"),
            (scope, b"query-two".as_slice(), "captures"),
            (scope, b"query-one".as_slice(), "reviews"),
        ] {
            assert!(matches!(
                resume(home.path(), reader, binding, section, &token),
                Err(Error::Expired)
            ));
        }
    }

    #[test]
    fn retention_keeps_newest_reusable_token_and_expiry_refuses_it() {
        let home = tempfile::tempdir().expect("private local metadata");
        let scope = [11; 32];
        let first = issue_with_limit(home.path(), scope, b"first", "captures", [1; 32], 2)
            .expect("first token");
        let middle = issue_with_limit(home.path(), scope, b"middle", "captures", [2; 32], 2)
            .expect("middle token");
        let latest = issue_with_limit(home.path(), scope, b"latest", "captures", [3; 32], 2)
            .expect("latest token");
        assert!(matches!(
            resume(home.path(), scope, b"first", "captures", &first),
            Err(Error::Expired)
        ));
        assert_eq!(
            resume(home.path(), scope, b"middle", "captures", &middle).expect("middle retained"),
            [2; 32]
        );
        assert_eq!(
            resume(home.path(), scope, b"latest", "captures", &latest).expect("newest retained"),
            [3; 32]
        );
        assert!(matches!(
            resume_at(
                home.path(),
                scope,
                b"latest",
                "captures",
                &latest,
                now().expect("clock") + TTL_SECONDS
            ),
            Err(Error::Expired)
        ));
    }
}
