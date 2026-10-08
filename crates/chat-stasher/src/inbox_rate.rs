//! Trusted rolling-hour accounting for authenticated pull attempts. A committed
//! reservation precedes decryption and sealing, so crashes and failed archive
//! proof cannot reset the quota. Sharing this file serializes local pullers;
//! separate machines still have independent quotas until distributed accounting
//! is implemented. Losing trusted state requires operator recovery.
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;

pub(crate) const WINDOW_SECS: u64 = 3600;

pub(crate) fn reserve(path: &Path, key: &str, now: u64, limit: usize) -> anyhow::Result<bool> {
    crate::test_identity_guard::refuse_fixture_write(&[key], path)?;
    let now = i64::try_from(now)?;
    let limit = i64::try_from(limit)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing rate state parent"))?;
    let metadata = std::fs::symlink_metadata(parent)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "unsafe rate state parent"
    );
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "unsafe rate state"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            file.metadata()?.permissions().mode() & 0o077 == 0,
            "rate state must be owner-only"
        );
    }
    let mut db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 0 {
        let tables: i64 = tx.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        anyhow::ensure!(tables == 0, "unrecognized rate state");
        tx.execute_batch(
            "CREATE TABLE inbox_rate_clock (key TEXT PRIMARY KEY, last_now INTEGER NOT NULL);
             CREATE TABLE inbox_rate_attempt (key TEXT NOT NULL, at INTEGER NOT NULL);
             CREATE INDEX inbox_rate_by_key ON inbox_rate_attempt(key, at);
             PRAGMA user_version=1;",
        )?;
    } else {
        anyhow::ensure!(version == 1, "unsupported rate state version");
    }
    let last: Option<i64> = tx
        .query_row(
            "SELECT last_now FROM inbox_rate_clock WHERE key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    anyhow::ensure!(
        last.is_none_or(|last| now >= last),
        "pull clock moved backwards"
    );
    let cutoff = now - WINDOW_SECS as i64;
    tx.execute(
        "DELETE FROM inbox_rate_attempt WHERE key=?1 AND at<=?2",
        params![key, cutoff],
    )?;
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM inbox_rate_attempt WHERE key=?1",
        [key],
        |row| row.get(0),
    )?;
    tx.execute("INSERT INTO inbox_rate_clock VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET last_now=excluded.last_now", params![key, now])?;
    let admitted = count < limit;
    if admitted {
        tx.execute(
            "INSERT INTO inbox_rate_attempt VALUES (?1, ?2)",
            params![key, now],
        )?;
    }
    tx.commit()?;
    // The database file was created before SQLite's transaction. Sync its
    // directory too before granting the reservation to the caller.
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(admitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_rolling_window_is_per_key_and_refuses_clock_regression() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("synthetic-rates.sqlite3");
        assert!(reserve(&path, "synthetic-a", 100, 2).unwrap());
        assert!(reserve(&path, "synthetic-a", 101, 2).unwrap());
        assert!(!reserve(&path, "synthetic-a", 3699, 2).unwrap());
        assert!(reserve(&path, "synthetic-b", 3699, 2).unwrap());
        assert!(reserve(&path, "synthetic-a", 3700, 2).unwrap());
        assert!(reserve(&path, "synthetic-a", 3699, 2).is_err());
        assert!(reserve(&path, "synthetic-a", 3701, 2).unwrap());
        assert!(!reserve(&path, "synthetic-a", 3701, 2).unwrap());
    }

    #[test]
    fn concurrent_connections_cannot_exceed_the_quota() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("synthetic-rates.sqlite3");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let results: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    reserve(&path, "synthetic-concurrent", 100, 3).unwrap()
                })
            })
            .collect();
        let admitted = results
            .into_iter()
            .map(|t| usize::from(t.join().unwrap()))
            .sum::<usize>();
        assert_eq!(admitted, 3);
        assert!(!reserve(&path, "synthetic-concurrent", 100, 3).unwrap());
    }

    #[test]
    fn corrupt_state_or_missing_parent_never_becomes_an_empty_quota() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("synthetic-rates.sqlite3");
        assert!(reserve(&path, "synthetic-key", 100, 1).unwrap());
        std::fs::write(&path, b"synthetic-corruption").unwrap();
        assert!(reserve(&path, "synthetic-key", 100, 1).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"synthetic-corruption");
        assert!(reserve(&root.path().join("missing/state"), "synthetic-key", 100, 1).is_err());
        assert!(!root.path().join("missing").exists());
    }

    #[test]
    fn missing_accounting_table_never_resets_a_used_quota() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("synthetic-rates.sqlite3");
        assert!(reserve(&path, "synthetic-key", 100, 1).unwrap());
        Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TABLE inbox_rate_attempt")
            .unwrap();
        assert!(reserve(&path, "synthetic-key", 101, 1).is_err());
    }

    #[test]
    fn unsafe_paths_are_refused_on_every_platform() {
        let root = tempfile::tempdir().unwrap();
        assert!(reserve(root.path(), "synthetic-key", 100, 1).is_err());
        #[cfg(unix)]
        {
            let real = root.path().join("synthetic-real.sqlite3");
            let link = root.path().join("synthetic-link.sqlite3");
            assert!(reserve(&real, "synthetic-key", 100, 1).unwrap());
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(reserve(&link, "synthetic-key", 100, 1).is_err());
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(reserve(&real, "synthetic-key", 100, 1).is_err());
        }
        // Windows symlink creation requires privileges; the directory refusal
        // above exercises its non-regular-file boundary without those rights.
    }
}
