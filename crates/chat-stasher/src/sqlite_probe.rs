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
//! Byte scope is part of the contract too: a live SQLite store is its `.db`
//! file *plus* its `-wal`/`-shm` siblings. Both consumers measure exactly this
//! set — counting only `.db` silently under-reports the store by the size of
//! the write-ahead log.

use rusqlite::{Connection, OpenFlags};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Columns the session table must carry (in any order) before we claim we can
/// enumerate sessions inside a store. Narrow on purpose: a changed schema is
/// reported loudly, never silently turned into a fabricated count of zero.
pub const EXPECTED_SESSION_COLUMNS: [&str; 3] = ["id", "time_created", "time_updated"];

/// Outcome of enumerating sessions inside one SQLite database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteSessionProbe {
    /// `session` table recognised, stats read. `earliest`/`latest` are the
    /// `min`/`max` of `time_created`.
    Known {
        count: u64,
        earliest: Option<SystemTime>,
        latest: Option<SystemTime>,
    },
    /// SQLite readable but the `session` table has a different shape. This is
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
/// connection). Shared by the doctor footprint walk and the scanner so both
/// tables report the same count *and* the same byte scope.
pub fn probe_sqlite_store(db: &Path) -> SqliteStoreProbe {
    SqliteStoreProbe {
        total_bytes: sqlite_store_bytes(db),
        sessions: probe_sqlite_sessions(db),
    }
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

/// Open `db` read-only and count rows in `session`.
///
/// A missing or changed schema is reported loudly via
/// [`SqliteSessionProbe::SchemaMismatch`] instead of becoming the old silent
/// `N/A` — and never a fake `0`.
pub fn probe_sqlite_sessions(db: &Path) -> SqliteSessionProbe {
    let uri = format!("file:{}?mode=ro", db.display());
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = match Connection::open_with_flags(uri, flags) {
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
    let session_columns = match sqlite_table_columns(&conn, "session") {
        Ok(columns) => columns,
        Err(e) => {
            return SqliteSessionProbe::ReadFailed {
                error: format!("读取 schema 失败: {e}"),
            };
        }
    };
    if !EXPECTED_SESSION_COLUMNS
        .iter()
        .all(|expected_column| session_columns.iter().any(|c| c == expected_column))
    {
        return SqliteSessionProbe::SchemaMismatch { actual };
    }

    let stats = conn.query_row(
        "SELECT count(*), min(time_created), max(time_created) FROM \"session\"",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        },
    );
    match stats {
        Ok((count, earliest, latest)) if count >= 0 => SqliteSessionProbe::Known {
            count: count as u64,
            earliest: earliest.and_then(sqlite_millis_to_system_time),
            latest: latest.and_then(sqlite_millis_to_system_time),
        },
        Ok((count, _, _)) => SqliteSessionProbe::ReadFailed {
            error: format!("SQLite 返回了负会话数: {count}"),
        },
        Err(e) => SqliteSessionProbe::ReadFailed {
            error: format!("读取 session 统计失败: {e}"),
        },
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
}
