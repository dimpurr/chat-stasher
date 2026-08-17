//! Shared read-only SQLite probing.
//!
//! This is the *single* implementation of "how do we count sessions inside a
//! SQLite store" and "how big is a SQLite store on disk". Both the doctor
//! footprint table and the registry-driven scanner table call into this module,
//! so the two numbers `doctor` prints for the same harness can never drift
//! apart again — that exact disagreement (opencode 会话 243 in the footprint
//! table vs 会话=0 in the registry table) was a real bug, because the SQLite
//! enumeration was wired into only one of the two paths.
//!
//! Since B27 the probe is schema-driven: each harness's registry cell declares
//! which table holds one row per session, which key pattern selects session
//! rows (Cursor's `cursorDiskKV.composerData:%`), and where the per-session
//! timestamp lives (`session.time_created`, `session_docs.updated_at`, or the
//! `createdAt` field inside Cursor's JSON `value`). A changed schema is still
//! reported loudly, never silently turned into a fabricated count of zero.
//!
//! Byte scope is part of the contract too: a live SQLite store is its `.db`
//! file *plus* its `-wal`/`-shm` siblings. Both consumers measure exactly this
//! set — counting only `.db` silently under-reports the store by the size of
//! the write-ahead log.
//!
//! Read-only guarantee: every connection opens with `mode=ro`; when that fails
//! because a WAL-mode store has no `-shm` yet (SQLite read-only WAL cannot
//! create the shared-memory file it needs — observed on Grok's
//! `session_search.sqlite`), we retry with `mode=ro&immutable=1`, which reads
//! only the main file and writes nothing. A read-only probe therefore never
//! creates or touches `-wal`/`-shm`, so the user's live Cursor/Grok stores
//! stay byte-identical.

use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Columns the opencode `session` table must carry (in any order) before we
/// claim we can enumerate its sessions. Kept for compatibility with the
/// pre-B27 default schema.
pub const EXPECTED_SESSION_COLUMNS: [&str; 3] = ["id", "time_created", "time_updated"];

/// How one SQLite store keeps its sessions, resolved from the registry cell
/// (`data/harness-registry-v1.json`). Both the scanner and the doctor build
/// the exact same spec for a harness, so they can never use two different
/// queries against the same database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteSchemaSpec<'a> {
    /// Table that holds one row per session.
    pub table: &'a str,
    /// Columns that must all exist before we claim we can enumerate sessions.
    pub required_columns: Vec<&'a str>,
    /// Key/value-store selection: count only rows where `key_column` matches
    /// `key_prefix` with a trailing `%` (e.g. Cursor `composerData:%`).
    /// `None` = count all rows of `table`.
    pub key_column: Option<&'a str>,
    pub key_prefix: Option<&'a str>,
    /// Column holding a per-session timestamp for `min`/`max`, if any.
    pub time_column: Option<&'a str>,
    /// Alternative to `time_column`: read the field at this JSON path from the
    /// JSON stored in `json_value_column` (epoch **millis**, like opencode).
    pub json_value_column: Option<&'a str>,
    pub json_time_path: Option<&'a str>,
    /// When `time_column` stores Unix **seconds** (Grok's `updated_at` is
    /// seconds; opencode `time_created` and Cursor `createdAt` are millis).
    pub time_is_seconds: bool,
    /// Optional named qualification rule for JSON key/value stores.
    /// `cursor_composer` excludes archived/empty composers after the raw key
    /// prefix count, so callers can report both numbers.
    pub qualification: Option<&'a str>,
}

/// The pre-B27 default schema (opencode's `session` table, millis).
pub fn opencode_schema() -> SqliteSchemaSpec<'static> {
    SqliteSchemaSpec {
        table: "session",
        required_columns: EXPECTED_SESSION_COLUMNS.to_vec(),
        key_column: None,
        key_prefix: None,
        time_column: Some("time_created"),
        json_value_column: None,
        json_time_path: None,
        time_is_seconds: false,
        qualification: None,
    }
}

/// Build the schema spec for a harness from its registry cell. `None` when the
/// cell is not a recognised SQLite store (no `sql_table` declared) — the
/// caller then falls back to [`opencode_schema`], preserving pre-B27 behaviour
/// for the SQLite harnesses whose schema is not registry-declared.
pub fn spec_from_cell(cell: &crate::scanner::RegistryCell) -> Option<SqliteSchemaSpec<'_>> {
    if cell.format != "sqlite" {
        return None;
    }
    let table = cell.sql_table.as_deref()?;
    let required_columns: Vec<&str> = if cell.sql_required_columns.is_empty() {
        let mut v = Vec::new();
        if let Some(k) = cell.sql_key_column.as_deref() {
            v.push(k);
        }
        if let Some(t) = cell.sql_time_column.as_deref() {
            v.push(t);
        }
        if let Some(vc) = cell.sql_value_column.as_deref() {
            v.push(vc);
        }
        v
    } else {
        cell.sql_required_columns
            .iter()
            .map(String::as_str)
            .collect()
    };
    Some(SqliteSchemaSpec {
        table,
        required_columns,
        key_column: cell.sql_key_column.as_deref(),
        key_prefix: cell.sql_key_pattern.as_deref(),
        time_column: cell.sql_time_column.as_deref(),
        json_value_column: cell.sql_value_column.as_deref(),
        json_time_path: cell.sql_time_json_path.as_deref(),
        time_is_seconds: cell.sql_time_value_is_seconds,
        qualification: cell.sql_qualification.as_deref(),
    })
}

/// Outcome of enumerating sessions inside one SQLite database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteSessionProbe {
    /// Session table recognised, stats read. `earliest`/`latest` are the
    /// `min`/`max` of the schema-declared time source.
    Known {
        /// Number of rows selected by the key/table predicate before any
        /// JSON qualification rule is applied.
        candidate_count: u64,
        count: u64,
        earliest: Option<SystemTime>,
        latest: Option<SystemTime>,
    },
    /// SQLite readable but the session table has a different shape. This is
    /// *not* "0 sessions" — it is "cannot enumerate", and the caller must say
    /// so instead of printing a fake zero.
    SchemaMismatch { actual: String },
    /// The database could not be opened / queried at all.
    ReadFailed { error: String },
}

/// Everything known about one SQLite store: total on-disk footprint and the
/// session enumeration result. One call, one source of truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteStoreProbe {
    /// `.db` + `.db-wal` + `.db-shm` bytes when present.
    pub total_bytes: u64,
    pub sessions: SqliteSessionProbe,
}

/// Probe one SQLite store, read-only (`mode=ro`, never a write-capable
/// connection), using the default opencode schema. Shared by the doctor
/// footprint walk and the scanner so both tables report the same count *and*
/// the same byte scope.
pub fn probe_sqlite_store(db: &Path) -> SqliteStoreProbe {
    probe_sqlite_store_with(db, &opencode_schema())
}

/// [`probe_sqlite_store`] with an explicit schema spec (registry-declared).
pub fn probe_sqlite_store_with(db: &Path, spec: &SqliteSchemaSpec) -> SqliteStoreProbe {
    SqliteStoreProbe {
        total_bytes: sqlite_store_bytes(db),
        sessions: probe_sqlite_sessions_with(db, spec),
    }
}

/// Probe Cursor's pre-global-storage layout. Each immediate workspace directory
/// may contain `state.vscdb`; its `ItemTable` stores either the
/// `composer.composerData` object (usually with an `allComposers` array) or an
/// `allComposers` value directly. Every database is opened through the same
/// strict read-only URI as the global probe.
pub fn probe_cursor_legacy_workspace_storage(workspace_storage: &Path) -> Option<SqliteStoreProbe> {
    if !workspace_storage.is_dir() {
        return None;
    }

    let mut saw_database = false;
    let mut saw_composer_value = false;
    let mut total_bytes = 0u64;
    let mut candidate_count = 0u64;
    let mut count = 0u64;
    let mut earliest: Option<SystemTime> = None;
    let mut latest: Option<SystemTime> = None;

    let entries = fs::read_dir(workspace_storage).ok()?;
    for entry in entries.flatten() {
        let workspace = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let db = workspace.join("state.vscdb");
        if !db.is_file() {
            continue;
        }
        saw_database = true;
        total_bytes += sqlite_store_bytes(&db);
        let Ok(conn) = open_readonly(&db) else {
            continue;
        };
        let value = conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key IN ('composer.composerData', 'allComposers') ORDER BY CASE key WHEN 'composer.composerData' THEN 0 ELSE 1 END LIMIT 1",
                [],
                |row| match row.get_ref(0)? {
                    ValueRef::Text(bytes) | ValueRef::Blob(bytes) => Ok(bytes.to_vec()),
                    ValueRef::Null => Ok(Vec::new()),
                    _ => Err(rusqlite::Error::InvalidColumnType(
                        0,
                        "value".to_string(),
                        rusqlite::types::Type::Integer,
                    )),
                },
            )
            .ok();
        let Some(value) = value else {
            continue;
        };
        saw_composer_value = true;
        let Ok(json) = serde_json::from_slice::<Value>(&value) else {
            continue;
        };
        let Some(composers) = json
            .get("allComposers")
            .and_then(Value::as_array)
            .or_else(|| json.as_array())
        else {
            continue;
        };
        for composer in composers {
            candidate_count += 1;
            if !cursor_legacy_composer_is_qualified(composer) {
                continue;
            }
            count += 1;
            if let Some(created_at) = composer.get("createdAt").and_then(Value::as_i64) {
                if let Some(timestamp) = sqlite_millis_to_system_time(created_at) {
                    earliest = Some(earliest.map_or(timestamp, |old| old.min(timestamp)));
                    latest = Some(latest.map_or(timestamp, |old| old.max(timestamp)));
                }
            }
        }
    }

    if !saw_database || !saw_composer_value {
        return None;
    }
    Some(SqliteStoreProbe {
        total_bytes,
        sessions: SqliteSessionProbe::Known {
            candidate_count,
            count,
            earliest,
            latest,
        },
    })
}

/// Total on-disk bytes of a SQLite store: the main file plus `-wal` and
/// `-shm` siblings when they exist.
pub fn sqlite_store_bytes(db: &Path) -> u64 {
    let mut total = 0u64;
    for p in sqlite_store_files(db) {
        if let Ok(md) = fs::metadata(&p) {
            total += md.len();
        }
    }
    total
}

/// The files that make up one SQLite store: `.db`, `.db-wal`, `.db-shm`.
fn sqlite_store_files(db: &Path) -> Vec<PathBuf> {
    vec![db.to_path_buf(), sidecar(db, "-wal"), sidecar(db, "-shm")]
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// True when the SQLite header marks the store as WAL journal mode (the file
/// format read/write version bytes at offsets 18/19 are both `2`; rollback
/// journal stores use `1`). Read straight from the file — no SQLite connection
/// involved, so zero side effects.
fn db_is_wal(db: &Path) -> bool {
    let mut hdr = [0u8; 100];
    let read_ok = fs::File::open(db)
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut hdr)
        })
        .is_ok();
    read_ok && hdr[18] == 2 && hdr[19] == 2
}

/// Open `db` strictly read-only, guaranteeing no file is ever created next to
/// the store (so the user's live Cursor/Grok databases stay byte-identical).
///
/// SQLite's read-only WAL access needs the store's `-shm` file; when it does
/// not exist a read-only connection *creates* `-shm`/`-wal` instead of
/// failing (observed with rusqlite's bundled SQLite on Grok's
/// `session_search.sqlite`). That is exactly the side effect we must not have,
/// so:
///   * WAL store with no `-shm` ⇒ open `mode=ro&immutable=1` (reads only the
///     main file, touches nothing; may not see uncheckpointed WAL content, but
///     a live WAL writer keeps its sidecars, so this case means "not live"),
///   * everything else (rollback-journal stores, live WAL stores whose
///     sidecars already exist) ⇒ `mode=ro` (correct on live stores, creates
///     nothing new).
fn open_readonly(db: &Path) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let mut uri = format!("file:{}?mode=ro", db.display());
    if db_is_wal(db) && !sidecar(db, "-shm").exists() {
        uri = format!("file:{}?mode=ro&immutable=1", db.display());
    }
    Connection::open_with_flags(uri, flags)
}

/// Open `db` read-only and enumerate sessions per the schema spec.
///
/// A missing or changed schema is reported loudly via
/// [`SqliteSessionProbe::SchemaMismatch`] instead of becoming the old silent
/// `N/A` — and never a fake `0`.
pub fn probe_sqlite_sessions(db: &Path) -> SqliteSessionProbe {
    probe_sqlite_sessions_with(db, &opencode_schema())
}

/// [`probe_sqlite_sessions`] with an explicit schema spec.
pub fn probe_sqlite_sessions_with(db: &Path, spec: &SqliteSchemaSpec) -> SqliteSessionProbe {
    let conn = match open_readonly(db) {
        Ok(conn) => conn,
        Err(e) => {
            return SqliteSessionProbe::ReadFailed {
                error: format!("只读打开失败: {e}"),
            };
        }
    };
    if let Err(e) = conn.busy_timeout(Duration::from_secs(2)) {
        return SqliteSessionProbe::ReadFailed {
            error: format!("设置只读查询超时失败: {e}"),
        };
    }

    let actual = sqlite_schema_summary(&conn);
    let columns = match sqlite_table_columns(&conn, spec.table) {
        Ok(columns) => columns,
        Err(e) => {
            return SqliteSessionProbe::ReadFailed {
                error: format!("读取 schema 失败: {e}"),
            };
        }
    };
    if !spec
        .required_columns
        .iter()
        .all(|expected_column| columns.iter().any(|c| c == expected_column))
    {
        return SqliteSessionProbe::SchemaMismatch { actual };
    }

    let candidate_where_sql = match (spec.key_column, spec.key_prefix) {
        (Some(col), Some(prefix)) => {
            format!(" WHERE \"{col}\" LIKE '{}'", prefix.replace('\'', "''"))
        }
        _ => String::new(),
    };
    let where_sql = qualification_where_sql(spec, &candidate_where_sql);
    let candidate_count_sql = format!(
        "SELECT count(*) FROM \"{}\"{candidate_where_sql}",
        spec.table
    );
    let count_sql = format!("SELECT count(*) FROM \"{}\"{where_sql}", spec.table);
    let time_sql = time_expression(spec).map(|expr| {
        // `json_extract` errors (does not return NULL) on malformed JSON, so a
        // key/value store whose rows carry junk must not make the *whole*
        // probe fail — restrict the time extraction to well-formed rows only.
        // The count above still covers every row, exactly like `count(*)`.
        let json_guard = if spec.json_time_path.is_some() {
            format!(
                " AND json_valid(\"{}\")=1",
                spec.json_value_column.unwrap_or("value")
            )
        } else {
            String::new()
        };
        format!(
            "SELECT {expr} FROM \"{}\"{where_sql}{json_guard}",
            spec.table
        )
    });

    let candidate_count = match conn.query_row(&candidate_count_sql, [], |row| row.get::<_, i64>(0))
    {
        Ok(c) if c >= 0 => c as u64,
        Ok(c) => {
            return SqliteSessionProbe::ReadFailed {
                error: format!("SQLite 返回了负候选会话数: {c}"),
            }
        }
        Err(e) => {
            return SqliteSessionProbe::ReadFailed {
                error: format!("读取 {} 表候选统计失败: {e}", spec.table),
            }
        }
    };
    let count = match conn.query_row(&count_sql, [], |row| row.get::<_, i64>(0)) {
        Ok(c) if c >= 0 => c as u64,
        Ok(c) => {
            return SqliteSessionProbe::ReadFailed {
                error: format!("SQLite 返回了负会话数: {c}"),
            }
        }
        Err(e) => {
            return SqliteSessionProbe::ReadFailed {
                error: format!("读取 {} 表统计失败: {e}", spec.table),
            }
        }
    };
    let (earliest, latest) = match time_sql {
        Some(sql) => match conn.query_row(&sql, [], |row| {
            Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?))
        }) {
            Ok((earliest, latest)) => (
                earliest.and_then(|v| convert_epoch(v, spec.time_is_seconds)),
                latest.and_then(|v| convert_epoch(v, spec.time_is_seconds)),
            ),
            Err(e) => {
                return SqliteSessionProbe::ReadFailed {
                    error: format!("读取 {} 表时间统计失败: {e}", spec.table),
                }
            }
        },
        None => (None, None),
    };

    SqliteSessionProbe::Known {
        candidate_count,
        count,
        earliest,
        latest,
    }
}

/// SQL predicate for the Cursor composer qualification rule. The initial
/// candidate predicate remains separate so the probe can expose before/after
/// counts. `json_valid` makes malformed values in a key/value store ineligible
/// without allowing one bad row to abort the whole count.
fn qualification_where_sql(spec: &SqliteSchemaSpec, candidate_where_sql: &str) -> String {
    if spec.qualification != Some("cursor_composer") {
        return candidate_where_sql.to_string();
    }
    let value = spec.json_value_column.unwrap_or("value");
    format!(
        "{candidate_where_sql} AND json_valid(\"{value}\")=1 \
         AND COALESCE(CAST(json_extract(\"{value}\", '$.isArchived') AS INTEGER), 0) <> 1 \
         AND (\
           (json_type(\"{value}\", '$.fullConversationHeadersOnly')='array' \
            AND json_array_length(\"{value}\", '$.fullConversationHeadersOnly') > 0) \
           OR CAST(COALESCE(json_extract(\"{value}\", '$.promptTokenBreakdown.totalUsedTokens'), 0) AS INTEGER) > 0\
         )"
    )
}

/// Qualification for the legacy `ItemTable`/`allComposers` structure.
///
/// This is intentionally *not* the global `cursor_composer` rule above:
/// legacy composer objects carry their messages in `conversation`, while the
/// global `cursorDiskKV` rows carry `fullConversationHeadersOnly` and token
/// breakdown fields. A non-empty conversation is the structure-specific proof
/// that the legacy composer has content; empty metadata-only composers are not
/// sessions for the fallback count.
fn cursor_legacy_composer_is_qualified(value: &Value) -> bool {
    if value
        .get("isArchived")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }
    value
        .get("conversation")
        .and_then(Value::as_array)
        .is_some_and(|messages| !messages.is_empty())
}

/// `min, max` SQL fragment for the schema's time source, or `None` when the
/// schema declares no timestamp (Cursor's table has no time column at all —
/// the JSON-path form is used instead).
fn time_expression(spec: &SqliteSchemaSpec) -> Option<String> {
    if let Some(col) = spec.time_column {
        Some(format!("min(\"{col}\"), max(\"{col}\")"))
    } else if let (Some(vcol), Some(path)) = (spec.json_value_column, spec.json_time_path) {
        let expr = format!("json_extract(\"{vcol}\", '{}')", path.replace('\'', "''"));
        Some(format!("min({expr}), max({expr})"))
    } else {
        None
    }
}

/// Convert a stored epoch to a `SystemTime`. Millis (opencode `time_created`,
/// Cursor `createdAt`) or seconds (Grok `updated_at`), chosen by the schema.
fn convert_epoch(v: i64, is_seconds: bool) -> Option<SystemTime> {
    if is_seconds {
        let duration = Duration::from_secs(v.unsigned_abs());
        if v >= 0 {
            UNIX_EPOCH.checked_add(duration)
        } else {
            UNIX_EPOCH.checked_sub(duration)
        }
    } else {
        sqlite_millis_to_system_time(v)
    }
}

/// Ordered column names of one table (`pragma_table_info`).
fn sqlite_table_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
    let rows = stmt.query_map([table], |row| row.get::<_, String>(0))?;
    rows.collect()
}

/// Schema-only summary used in loud mismatch diagnostics; never selects rows
/// from application tables.
pub fn sqlite_schema_summary(conn: &Connection) -> String {
    let names = match conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        }) {
        Ok(names) => names,
        Err(e) => return format!("<读取表名失败: {e}>"),
    };

    if names.is_empty() {
        return "<无用户表>".to_string();
    }

    names
        .iter()
        .map(|name| match sqlite_table_columns(conn, name) {
            Ok(columns) => format!("{name}({})", columns.join(", ")),
            Err(e) => format!("{name}(<读取列失败: {e}>)"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn sqlite_millis_to_system_time(millis: i64) -> Option<SystemTime> {
    let duration = Duration::from_millis(millis.unsigned_abs());
    if millis >= 0 {
        UNIX_EPOCH.checked_add(duration)
    } else {
        UNIX_EPOCH.checked_sub(duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    /// A real SQLite store with a recognised `session` table.
    fn make_session_store(dir: &Path, n: u64) -> PathBuf {
        let db = dir.join("opencode.db");
        if let Some(p) = db.parent() {
            fs::create_dir_all(p).unwrap();
        }
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session(id TEXT PRIMARY KEY, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL)",
        )
        .unwrap();
        for i in 0..n {
            conn.execute(
                "INSERT INTO session (id, time_created, time_updated) VALUES (?1, ?2, ?3)",
                rusqlite::params![format!("s{i}"), i as i64, i as i64],
            )
            .unwrap();
        }
        drop(conn);
        db
    }

    /// Byte scope includes the `-wal` / `-shm` sidecars — counting only `.db`
    /// would silently under-report a live SQLite store.
    #[test]
    fn store_bytes_include_wal_and_shm_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_session_store(dir.path(), 2);
        write(&sidecar(&db, "-wal"), "wal-bytes");
        write(&sidecar(&db, "-shm"), "shm-bytes");
        let db_len = fs::metadata(&db).unwrap().len();
        assert_eq!(
            sqlite_store_bytes(&db),
            db_len + "wal-bytes".len() as u64 + "shm-bytes".len() as u64
        );
    }

    /// Recognised schema → real count, not zero.
    #[test]
    fn probe_counts_known_session_table_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_session_store(dir.path(), 3);
        let info = probe_sqlite_store(&db);
        assert_eq!(
            info.sessions,
            SqliteSessionProbe::Known {
                candidate_count: 3,
                count: 3,
                earliest: sqlite_millis_to_system_time(0),
                latest: sqlite_millis_to_system_time(2),
            }
        );
    }

    /// Unrecognised schema must be a loud `SchemaMismatch`, never a fake 0.
    #[test]
    fn unknown_schema_is_schema_mismatch_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.vscdb");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable(key TEXT, value TEXT)")
            .unwrap();
        drop(conn);
        let info = probe_sqlite_store(&db);
        assert!(matches!(
            info.sessions,
            SqliteSessionProbe::SchemaMismatch { .. }
        ));
    }

    /// Missing file → `ReadFailed`, never a fabricated zero.
    #[test]
    fn missing_db_is_read_failed_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        let info = probe_sqlite_store(&dir.path().join("nope.db"));
        assert!(matches!(
            info.sessions,
            SqliteSessionProbe::ReadFailed { .. }
        ));
    }

    /// A Cursor-shaped store: key/value table, session rows selected by
    /// `composerData:%`, times pulled from `createdAt` inside the JSON value.
    #[test]
    fn cursor_schema_counts_key_prefix_and_extracts_json_time() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.vscdb");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
        )
        .unwrap();
        let sessions = [
            (
                "composerData:aaaaaaaa-1111",
                r#"{"composerId":"a","createdAt":1751779149032,"fullConversationHeadersOnly":[{}]}"#,
            ),
            (
                "composerData:bbbbbbbb-2222",
                r#"{"composerId":"b","createdAt":1752849959504,"isArchived":true,"fullConversationHeadersOnly":[{}]}"#,
            ),
            ("inlineDiffs-123456", r#"{"n":1}"#), // must not count as a session
            (
                "composerData:cccccccc-3333",
                r#"{"composerId":"c","createdAt":1753000000000}"#,
            ), // empty composer
        ];
        for (key, value) in &sessions {
            conn.execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, value],
            )
            .unwrap();
        }
        drop(conn);

        let spec = SqliteSchemaSpec {
            table: "cursorDiskKV",
            required_columns: vec!["key", "value"],
            key_column: Some("key"),
            key_prefix: Some("composerData:%"),
            time_column: None,
            json_value_column: Some("value"),
            json_time_path: Some("$.createdAt"),
            time_is_seconds: false,
            qualification: Some("cursor_composer"),
        };
        let info = probe_sqlite_store_with(&db, &spec);
        assert_eq!(
            info.sessions,
            SqliteSessionProbe::Known {
                candidate_count: 3,
                count: 1,
                earliest: sqlite_millis_to_system_time(1751779149032),
                latest: sqlite_millis_to_system_time(1751779149032),
            }
        );
    }

    #[test]
    fn legacy_workspace_storage_counts_only_qualified_composers() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspaceStorage").join("opaque-workspace");
        fs::create_dir_all(&workspace).unwrap();
        let db = workspace.join("state.vscdb");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value TEXT)")
            .unwrap();
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params![
                "composer.composerData",
                r#"{"allComposers":[{"createdAt":1751779149032,"conversation":[{}]},{"createdAt":1752849959504,"isArchived":true,"conversation":[{}]},{"createdAt":1753000000000}]}"#
            ],
        )
        .unwrap();
        drop(conn);

        let info =
            probe_cursor_legacy_workspace_storage(&dir.path().join("workspaceStorage")).unwrap();
        assert_eq!(
            info.sessions,
            SqliteSessionProbe::Known {
                candidate_count: 3,
                count: 1,
                earliest: sqlite_millis_to_system_time(1751779149032),
                latest: sqlite_millis_to_system_time(1751779149032),
            }
        );
    }

    /// A Grok-shaped store: `session_docs`, all rows, `updated_at` in seconds.
    #[test]
    fn grok_schema_counts_all_rows_and_treats_time_as_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("session_search.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL, updated_at INTEGER NOT NULL, title TEXT NOT NULL, content TEXT NOT NULL, content_hash TEXT NOT NULL)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_docs (session_id, cwd, updated_at, title, content, content_hash) VALUES (?1,?2,?3,?4,?5,?6)",
            rusqlite::params!["s1", "/tmp/x", 1784924765i64, "t", "c", "h"],
        )
        .unwrap();
        drop(conn);

        let spec = SqliteSchemaSpec {
            table: "session_docs",
            required_columns: vec!["session_id", "updated_at"],
            key_column: None,
            key_prefix: None,
            time_column: Some("updated_at"),
            json_value_column: None,
            json_time_path: None,
            time_is_seconds: true,
            qualification: None,
        };
        let info = probe_sqlite_store_with(&db, &spec);
        assert_eq!(
            info.sessions,
            SqliteSessionProbe::Known {
                candidate_count: 1,
                count: 1,
                earliest: UNIX_EPOCH.checked_add(Duration::from_secs(1784924765)),
                latest: UNIX_EPOCH.checked_add(Duration::from_secs(1784924765)),
            }
        );
    }

    /// A WAL-mode store without `-shm` must still probe (read-only `mode=ro`
    /// fails for it, so the immutable fallback must kick in) and must not
    /// create sidecar files in the process.
    #[test]
    fn wal_store_without_shm_probes_immutable_and_creates_no_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("session_search.sqlite");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL").unwrap();
            conn.execute_batch(
                "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, updated_at INTEGER NOT NULL)",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_docs (session_id, updated_at) VALUES (?1, ?2)",
                rusqlite::params!["s1", 1784924765i64],
            )
            .unwrap();
            // The write-connection close normally checkpoints + removes the
            // sidecars; force the "app closed cleanly, no sidecars" real state.
            drop(conn);
        }
        for suffix in ["-wal", "-shm"] {
            let _ = fs::remove_file(sidecar(&db, suffix));
        }

        let spec = SqliteSchemaSpec {
            table: "session_docs",
            required_columns: vec!["session_id", "updated_at"],
            key_column: None,
            key_prefix: None,
            time_column: Some("updated_at"),
            json_value_column: None,
            json_time_path: None,
            time_is_seconds: true,
            qualification: None,
        };
        let info = probe_sqlite_store_with(&db, &spec);
        assert!(matches!(
            info.sessions,
            SqliteSessionProbe::Known { count: 1, .. }
        ));
        // Zero side effect: probing must not recreate the sidecars.
        assert!(!sidecar(&db, "-wal").exists(), "probe must not create -wal");
        assert!(!sidecar(&db, "-shm").exists(), "probe must not create -shm");
    }

    /// spec_from_cell: a declared sqlite cell yields a spec; anything else None.
    #[test]
    fn spec_from_cell_resolves_declared_sqlite_cells() {
        let cell: crate::scanner::RegistryCell = serde_json::from_value(serde_json::json!({
            "template": "~/x.db",
            "format": "sqlite",
            "confidence": "本机实测",
            "source": "s",
            "sql_table": "cursorDiskKV",
            "sql_required_columns": ["key", "value"],
            "sql_key_column": "key",
            "sql_key_pattern": "composerData:%",
            "sql_value_column": "value",
            "sql_time_json_path": "$.createdAt"
        }))
        .unwrap();
        let spec = spec_from_cell(&cell).unwrap();
        assert_eq!(spec.table, "cursorDiskKV");
        assert_eq!(spec.key_prefix, Some("composerData:%"));
        assert_eq!(spec.json_time_path, Some("$.createdAt"));

        let non_sqlite: crate::scanner::RegistryCell = serde_json::from_value(serde_json::json!({
            "template": "~/x.jsonl",
            "format": "jsonl",
            "confidence": "源码确认",
            "source": "s"
        }))
        .unwrap();
        assert!(spec_from_cell(&non_sqlite).is_none());

        let sqlite_no_table: crate::scanner::RegistryCell =
            serde_json::from_value(serde_json::json!({
                "template": "~/x.db",
                "format": "sqlite",
                "confidence": "源码确认",
                "source": "s"
            }))
            .unwrap();
        assert!(spec_from_cell(&sqlite_no_table).is_none());
    }
}
