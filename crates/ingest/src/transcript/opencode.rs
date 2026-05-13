// SPDX-License-Identifier: Apache-2.0
//! Load OpenCode sessions from the local SQLite store into normalized
//! [`Transcript`]s.
//!
//! # On-disk shape
//!
//! OpenCode stores every session in a single SQLite file, by default
//! `~/.local/share/opencode/opencode.db` (WAL mode; `-shm` and `-wal`
//! sidecars may be present). The tables we rely on are:
//!
//! | Table     | Columns we touch                                          |
//! |-----------|-----------------------------------------------------------|
//! | `session` | `id`, `directory`, `time_created`, `time_updated`         |
//! | `message` | `session_id`, `time_created`                              |
//! | `part`    | `session_id`, `time_created`, `data`                      |
//!
//! `time_*` columns are Unix timestamps in **milliseconds**. `directory`
//! is the absolute cwd the session ran against — OpenCode calls it
//! "directory", not "cwd" — and is the same signal Codex's `session_meta.cwd`
//! gives us.
//!
//! `part.data` is a JSON blob; for tool-call parts it looks like:
//!
//! ```json
//! {
//!   "type": "tool",
//!   "tool": "write",            // or "edit", "read", "apply_patch", …
//!   "state": {
//!     "status": "completed",
//!     "input":  { "filePath": "/abs/path/to/file", ... },
//!     "output": "…"
//!   }
//! }
//! ```
//!
//! We only extract tool calls whose `tool` name is one of the file-
//! touching verbs — `write`/`edit`/`apply_patch` → `Write`, `read` →
//! `Read`. Grep/glob/bash live under the same schema but don't carry a
//! single `filePath`; they'd need command-string parsing like
//! [`super::codex`] does, which we defer until we see real demand for it.
//!
//! # Schema stability
//!
//! OpenCode knows third-party tools read this DB, so the schema is
//! relatively stable — but we still verify the columns we rely on exist
//! before querying. When the probe fails we log a **prominent** warning
//! naming the table/column so users can file a bug, and return an empty
//! transcript set rather than aborting the whole ingest run. The other
//! providers (Claude, Codex) keep working.
//!
//! # Concurrency
//!
//! The DB may be open for writes by a running `opencode` process. We
//! open with `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI` and set a short
//! busy timeout. If the DB is locked we warn and skip — never block the
//! ingest run.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::{debug, warn};

use super::types::{FileTouch, Provider, TouchKind, Transcript};

/// Relative path from the OpenCode home to the SQLite file. Exposed as a
/// constant so tests can construct a matching layout without duplicating
/// the literal.
pub const DB_RELATIVE_PATH: &str = "opencode.db";

/// Load every OpenCode session whose `directory` is (or contains, or is
/// under) `repo_root`. Never errors — problems are logged and result in
/// an empty vec so the surrounding ingest pass keeps moving.
pub fn load(opencode_home: &Path, repo_root: &Path) -> Vec<Transcript> {
    let db_path = opencode_home.join(DB_RELATIVE_PATH);
    if !db_path.exists() {
        debug!(
            db = %db_path.display(),
            "no OpenCode database at the configured path; skipping provider",
        );
        return Vec::new();
    }

    let conn = match open_read_only(&db_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                db = %db_path.display(),
                error = %e,
                "OpenCode DB failed to open read-only. \
                 if OpenCode is installed this likely means the file is \
                 locked or unreadable — continuing without OpenCode transcripts",
            );
            return Vec::new();
        }
    };

    if let Err(problem) = validate_schema(&conn) {
        warn!(
            db = %db_path.display(),
            problem = %problem,
            "OpenCode SQLite schema doesn't match what heddle-ingest expects. \
             this usually means OpenCode upgraded its internal layout. \
             please file an issue against heddle with your `opencode --version` \
             and the `problem` field above. continuing without OpenCode transcripts",
        );
        return Vec::new();
    }

    let sessions = match fetch_sessions(&conn) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                db = %db_path.display(),
                error = %e,
                "OpenCode session query failed. schema may have changed; \
                 continuing without OpenCode transcripts",
            );
            return Vec::new();
        }
    };

    let total = sessions.len();
    let matching: Vec<_> = sessions
        .into_iter()
        .filter(|s| directory_matches(&s.directory, repo_root))
        .collect();
    debug!(
        total_sessions = total,
        matching = matching.len(),
        repo = %repo_root.display(),
        "OpenCode session filter",
    );

    let mut out = Vec::with_capacity(matching.len());
    for s in matching {
        match load_session(&conn, &s, &db_path) {
            Ok(t) => out.push(t),
            Err(e) => warn!(
                session_id = %s.id,
                error = %e,
                "OpenCode session failed to load — skipping just this session",
            ),
        }
    }
    out
}

fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    // URI mode lets rusqlite handle WAL sidecars the way SQLite expects.
    // The `?mode=ro` query param is belt-and-braces alongside the flag.
    let uri = format!("file:{}?mode=ro", path.display());
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(&uri, flags)?;
    // Non-zero busy timeout — absorbs the brief writer locks that happen
    // while OpenCode is running. 500ms is well under any ingest budget.
    let _ = conn.busy_timeout(std::time::Duration::from_millis(500));
    Ok(conn)
}

/// Columns we read in each of the three tables. Miss one → bail loudly.
const SESSION_COLS: &[&str] = &["id", "directory", "time_created", "time_updated"];
const MESSAGE_COLS: &[&str] = &["session_id"];
const PART_COLS: &[&str] = &["session_id", "time_created", "data"];

fn validate_schema(conn: &Connection) -> Result<(), String> {
    for (table, wanted) in [
        ("session", SESSION_COLS),
        ("message", MESSAGE_COLS),
        ("part", PART_COLS),
    ] {
        let actual = table_columns(conn, table)
            .map_err(|e| format!("reading schema for table `{table}`: {e}"))?;
        if actual.is_empty() {
            return Err(format!("table `{table}` is missing"));
        }
        for col in wanted {
            if !actual.iter().any(|c| c == col) {
                return Err(format!(
                    "table `{table}` missing column `{col}` (found: {})",
                    actual.join(", ")
                ));
            }
        }
    }
    Ok(())
}

fn table_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    // `PRAGMA table_info` is the standard introspection path. Safe to
    // interpolate the table name because we only call it with
    // hard-coded string literals.
    let sql = format!("PRAGMA table_info(\"{table}\")");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(1))? // column 1 of table_info = name
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Debug)]
struct SessionRow {
    id: String,
    directory: PathBuf,
    time_created_ms: i64,
    time_updated_ms: i64,
}

fn fetch_sessions(conn: &Connection) -> rusqlite::Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare("SELECT id, directory, time_created, time_updated FROM session")?;
    let rows = stmt.query_map([], |r| {
        Ok(SessionRow {
            id: r.get(0)?,
            directory: PathBuf::from(r.get::<_, String>(1)?),
            time_created_ms: r.get(2)?,
            time_updated_ms: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// `true` if `dir` is inside `repo_root`, or `repo_root` is inside `dir`.
/// Mirrors [`super::cwd_inside`] so repo-root/crates and crates/child-dir
/// sessions both match.
fn directory_matches(dir: &Path, repo_root: &Path) -> bool {
    super::repo_matches_checkout(dir, repo_root)
}

fn load_session(conn: &Connection, s: &SessionRow, db_path: &Path) -> rusqlite::Result<Transcript> {
    let turn_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM message WHERE session_id = ?1",
        [&s.id],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT data, time_created FROM part \
         WHERE session_id = ?1 \
         ORDER BY time_created ASC, id ASC",
    )?;
    let rows = stmt.query_map([&s.id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;

    let mut touches = Vec::new();
    for row in rows {
        let (data, ts_ms) = row?;
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let Some(ts) = ms_to_utc(ts_ms) else {
            continue;
        };
        extract_touch(&v, ts, &mut touches);
    }

    // Prefer the session's own time bounds. If they're garbage we fall
    // back to the first/last touch timestamp; if those are also missing
    // we fall back to the created time for both ends so the matcher
    // still sees a (degenerate) window.
    let started = ms_to_utc(s.time_created_ms)
        .or_else(|| touches.first().map(|t| t.timestamp))
        .unwrap_or_else(Utc::now);
    let ended = ms_to_utc(s.time_updated_ms)
        .or_else(|| touches.last().map(|t| t.timestamp))
        .unwrap_or(started);

    Ok(Transcript {
        provider: Provider::OpenCode,
        session_id: s.id.clone(),
        // No per-session file exists; point at the DB so later code that
        // wants a source identifier has something meaningful. The
        // reasoning-extract `harvest` dispatch for OpenCode is a no-op
        // today, so nothing will try to read this as JSONL.
        source_path: db_path.to_path_buf(),
        cwd: Some(s.directory.clone()),
        started_at: started,
        ended_at: ended,
        turn_count: turn_count.max(0) as u32,
        files_touched: touches,
        starting_commit: None,
    })
}

/// Parse a `part.data` JSON object; append a touch when it's a
/// file-touching tool call we recognize.
fn extract_touch(v: &Value, ts: DateTime<Utc>, out: &mut Vec<FileTouch>) {
    if v.get("type").and_then(Value::as_str) != Some("tool") {
        return;
    }
    let tool = v.get("tool").and_then(Value::as_str).unwrap_or("");
    let kind = match tool {
        // Write-class. `apply_patch` is included because OpenCode exposes
        // it as a first-class tool alongside `write`/`edit`.
        "write" | "edit" | "apply_patch" => TouchKind::Write,
        "read" => TouchKind::Read,
        // Everything else (grep, glob, bash, skill, task, websearch,
        // webfetch, MCP tools, …) is either not a file touch or would
        // require per-tool command parsing. Silently skip — the matcher
        // handles sparse touch lists fine.
        _ => return,
    };
    let Some(path) = v.pointer("/state/input/filePath").and_then(Value::as_str) else {
        return;
    };
    out.push(FileTouch {
        path: PathBuf::from(path),
        timestamp: ts,
        kind,
    });
}

fn ms_to_utc(ms: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(ms)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use rusqlite::params;
    use tempfile::TempDir;

    use super::*;

    /// Create a fixture DB with the OpenCode-shaped schema plus the
    /// handful of columns the loader actually touches. Other columns are
    /// set to trivial defaults so NOT NULL constraints are satisfied.
    fn make_fixture_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                directory TEXT NOT NULL,
                title TEXT NOT NULL,
                version TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL
             );
             CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
             );
             CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
             );",
        )
        .unwrap();
        conn
    }

    fn insert_session(
        conn: &Connection,
        id: &str,
        directory: &str,
        created_ms: i64,
        updated_ms: i64,
    ) {
        conn.execute(
            "INSERT INTO session (id, project_id, directory, title, version, time_created, time_updated) \
             VALUES (?1, 'proj', ?2, 't', 'v', ?3, ?4)",
            params![id, directory, created_ms, updated_ms],
        )
        .unwrap();
    }

    fn insert_message(conn: &Connection, id: &str, session_id: &str, ts_ms: i64) {
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, '{}')",
            params![id, session_id, ts_ms],
        )
        .unwrap();
    }

    fn insert_part(conn: &Connection, id: &str, session_id: &str, ts_ms: i64, data: &str) {
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, data) \
             VALUES (?1, 'msg', ?2, ?3, ?4)",
            params![id, session_id, ts_ms, data],
        )
        .unwrap();
    }

    #[test]
    fn loads_sessions_whose_directory_matches_repo_family() {
        let tmp = TempDir::new().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let init = Command::new("git")
            .args(["init"])
            .arg(&main)
            .output()
            .expect("git init");
        assert!(init.status.success(), "git init failed: {init:?}");
        let config_name = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["config", "user.name", "Test User"])
            .output()
            .expect("git config user.name");
        assert!(
            config_name.status.success(),
            "git config failed: {config_name:?}"
        );
        let config_email = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .expect("git config user.email");
        assert!(
            config_email.status.success(),
            "git config failed: {config_email:?}"
        );
        std::fs::write(main.join("README.md"), "hello\n").unwrap();
        let add = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["add", "README.md"])
            .output()
            .expect("git add");
        assert!(add.status.success(), "git add failed: {add:?}");
        let commit = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["commit", "-m", "init"])
            .output()
            .expect("git commit");
        assert!(commit.status.success(), "git commit failed: {commit:?}");

        let linked = tmp.path().join("linked");
        let wt = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["worktree", "add"])
            .arg(&linked)
            .args(["-b", "linked"])
            .output()
            .expect("git worktree add");
        assert!(wt.status.success(), "git worktree add failed: {wt:?}");

        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let init_other = Command::new("git")
            .args(["init"])
            .arg(&other)
            .output()
            .expect("git init other");
        assert!(
            init_other.status.success(),
            "git init other failed: {init_other:?}"
        );

        let db_path = tmp.path().join("opencode.db");
        let conn = make_fixture_db(&db_path);
        insert_session(
            &conn,
            "SES_MAIN",
            &main.to_string_lossy(),
            1_000_000,
            2_000_000,
        );
        insert_session(
            &conn,
            "SES_LINKED",
            &linked.to_string_lossy(),
            1_000_000,
            2_000_000,
        );
        insert_session(
            &conn,
            "SES_OUT",
            &other.to_string_lossy(),
            1_000_000,
            2_000_000,
        );
        insert_message(&conn, "m1", "SES_MAIN", 1_500_000);
        insert_part(
            &conn,
            "p1",
            "SES_MAIN",
            1_500_000,
            &format!(
                r#"{{"type":"tool","tool":"write","state":{{"input":{{"filePath":"{}"}}}}}}"#,
                main.join("x.rs").display()
            ),
        );
        drop(conn);

        let got = load(tmp.path(), &main);
        let ids: Vec<_> = got.iter().map(|t| t.session_id.clone()).collect();
        assert!(ids.contains(&"SES_MAIN".to_string()), "ids: {ids:?}");
        assert!(
            ids.contains(&"SES_LINKED".to_string()),
            "linked worktree should also count: {ids:?}"
        );
        assert!(
            !ids.contains(&"SES_OUT".to_string()),
            "different repo must be filtered: {ids:?}"
        );
    }

    #[test]
    fn tool_parts_produce_file_touches_with_correct_kinds() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("opencode.db");
        let conn = make_fixture_db(&db_path);
        insert_session(&conn, "S", "/repo", 1_000_000, 5_000_000);
        for i in 0..5 {
            insert_message(&conn, &format!("m{i}"), "S", 1_000_000 + i);
        }
        insert_part(
            &conn,
            "p1",
            "S",
            2_000_000,
            r#"{"type":"tool","tool":"write","state":{"input":{"filePath":"/repo/a.rs"}}}"#,
        );
        insert_part(
            &conn,
            "p2",
            "S",
            3_000_000,
            r#"{"type":"tool","tool":"edit","state":{"input":{"filePath":"/repo/b.rs"}}}"#,
        );
        insert_part(
            &conn,
            "p3",
            "S",
            4_000_000,
            r#"{"type":"tool","tool":"read","state":{"input":{"filePath":"/repo/c.rs"}}}"#,
        );
        // Non-file tool: should be ignored.
        insert_part(
            &conn,
            "p4",
            "S",
            4_500_000,
            r#"{"type":"tool","tool":"grep","state":{"input":{"pattern":"foo"}}}"#,
        );
        // Non-tool part (e.g. a text message).
        insert_part(
            &conn,
            "p5",
            "S",
            4_600_000,
            r#"{"type":"text","text":"hello"}"#,
        );
        drop(conn);

        let mut got = load(tmp.path(), Path::new("/repo"));
        assert_eq!(got.len(), 1);
        let t = got.pop().unwrap();
        assert_eq!(t.provider, Provider::OpenCode);
        assert_eq!(t.turn_count, 5);
        let touches: Vec<_> = t
            .files_touched
            .iter()
            .map(|f| (f.path.clone(), f.kind))
            .collect();
        assert_eq!(
            touches,
            vec![
                (PathBuf::from("/repo/a.rs"), TouchKind::Write),
                (PathBuf::from("/repo/b.rs"), TouchKind::Write),
                (PathBuf::from("/repo/c.rs"), TouchKind::Read),
            ]
        );
    }

    #[test]
    fn session_time_bounds_use_session_row() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("opencode.db");
        let conn = make_fixture_db(&db_path);
        // 1,700,000,000,000 ms = 2023-11-14T22:13:20Z
        insert_session(&conn, "S", "/repo", 1_700_000_000_000, 1_700_000_060_000);
        drop(conn);

        let got = load(tmp.path(), Path::new("/repo"));
        let t = &got[0];
        assert_eq!(t.started_at.timestamp_millis(), 1_700_000_000_000);
        assert_eq!(t.ended_at.timestamp_millis(), 1_700_000_060_000);
    }

    #[test]
    fn missing_db_returns_empty_without_error() {
        let tmp = TempDir::new().unwrap();
        let got = load(tmp.path(), Path::new("/repo"));
        assert!(got.is_empty());
    }

    #[test]
    fn schema_mismatch_returns_empty_without_error() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("opencode.db");
        // Wrong schema: `session` table exists but `directory` is named
        // `cwd` — mirrors the kind of rename we're guarding against.
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                cwd TEXT NOT NULL,
                time_created INTEGER,
                time_updated INTEGER
             );
             CREATE TABLE message (session_id TEXT);
             CREATE TABLE part (session_id TEXT, time_created INTEGER, data TEXT);",
        )
        .unwrap();
        drop(conn);

        let got = load(tmp.path(), Path::new("/repo"));
        assert!(got.is_empty());
    }

    #[test]
    fn malformed_part_json_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("opencode.db");
        let conn = make_fixture_db(&db_path);
        insert_session(&conn, "S", "/repo", 1_000_000, 2_000_000);
        insert_part(&conn, "p1", "S", 1_500_000, "not-json");
        insert_part(
            &conn,
            "p2",
            "S",
            1_600_000,
            r#"{"type":"tool","tool":"write","state":{"input":{"filePath":"/repo/ok.rs"}}}"#,
        );
        drop(conn);

        let got = load(tmp.path(), Path::new("/repo"));
        let t = &got[0];
        assert_eq!(t.files_touched.len(), 1);
        assert_eq!(t.files_touched[0].path, PathBuf::from("/repo/ok.rs"));
    }

    #[test]
    fn extract_touch_ignores_unknown_tools_and_non_tool_parts() {
        let mut out = Vec::new();
        let ts = Utc::now();
        extract_touch(
            &serde_json::json!({"type":"text","text":"hi"}),
            ts,
            &mut out,
        );
        extract_touch(
            &serde_json::json!({"type":"tool","tool":"bash","state":{"input":{"command":"ls"}}}),
            ts,
            &mut out,
        );
        extract_touch(
            &serde_json::json!({"type":"tool","tool":"edit","state":{"input":{}}}),
            ts,
            &mut out,
        );
        assert!(out.is_empty(), "got: {out:?}");
    }
}