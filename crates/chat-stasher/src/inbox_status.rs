//! Read-only inbox observations and durable, content-free pull history.
//! Waiting is measured from the current backend, never inferred from history.
//! History contains counts and typed refusal reasons, never paths or payloads.
use crate::{
    inbox_config,
    remote_inbox::{PullReport, Refusal},
};
use rusqlite::{params, Connection, OpenFlags, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullState {
    Observed,
    Unknown,
}
impl PullState {
    fn label(&self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LastPull {
    pub at: u64,
    pub state: PullState,
    pub exit_code: u8,
    pub listed: Option<usize>,
    pub stored: Option<usize>,
    pub duplicates: Option<usize>,
    /// Validated captures with no platform account identity, including resends.
    pub missing_account: Option<usize>,
    pub refused: Option<BTreeMap<Refusal, usize>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub last_successful_pull: Option<u64>,
    pub last_pull: LastPull,
}

pub fn history_path(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    inbox_config::validate(name, "memory://validation")?;
    Ok(root.join(format!("{name}.status.sqlite3")))
}

fn safe(path: &Path, directory: bool) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink()
            && if directory {
                metadata.is_dir()
            } else {
                metadata.is_file()
            },
        "unsafe inbox status path"
    );
    if !directory {
        anyhow::ensure!(metadata.len() <= 1024 * 1024, "inbox status size limit");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            anyhow::ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "inbox status must be owner-only"
            );
        }
    }
    Ok(())
}

fn read_db(db: &Connection) -> anyhow::Result<History> {
    let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    anyhow::ensure!(version == 1, "unsupported inbox status version");
    let bytes: String = db.query_row("SELECT history FROM inbox_status WHERE id=1", [], |row| {
        row.get(0)
    })?;
    anyhow::ensure!(bytes.len() <= 65536, "inbox status size limit");
    Ok(serde_json::from_str(&bytes)?)
}

/// Never creates a file. Absence, corruption and unreadability remain distinct.
pub fn load(root: &Path, name: &str) -> anyhow::Result<Option<History>> {
    let path = history_path(root, name)?;
    safe(root, true)?;
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(_) => safe(&path, false)?,
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    read_db(&db).map(Some)
}

/// Serialize concurrent writers; preserve the last fully successful pass even
/// after a refusal or unreachable backend. A corrupt history is never replaced.
/// `None` means the pull could not observe the inbox, not a zero-object pass.
pub fn record(
    root: &Path,
    name: &str,
    now: u64,
    report: Option<&PullReport>,
) -> anyhow::Result<()> {
    crate::test_identity_guard::refuse_fixture_write(&[name], root)?;
    safe(root, true)?;
    let path = history_path(root, name)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let created = match options.open(&path) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e.into()),
    };
    safe(&path, false)?;
    let mut db = Connection::open(&path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let previous = if created {
        tx.execute_batch("CREATE TABLE inbox_status (id INTEGER PRIMARY KEY CHECK(id=1), history TEXT NOT NULL); PRAGMA user_version=1;")?;
        None
    } else {
        Some(read_db(&tx)?)
    };
    anyhow::ensure!(
        previous.as_ref().is_none_or(|h| now >= h.last_pull.at),
        "inbox status clock moved backwards"
    );
    let exit_code = report.map_or(3, PullReport::exit_status);
    let history = History {
        last_successful_pull: if exit_code == 0 {
            Some(now)
        } else {
            previous.and_then(|h| h.last_successful_pull)
        },
        last_pull: LastPull {
            at: now,
            state: if report.is_some() {
                PullState::Observed
            } else {
                PullState::Unknown
            },
            exit_code,
            listed: report.map(|r| r.waiting),
            stored: report.map(|r| r.stored),
            duplicates: report.map(|r| r.duplicates),
            missing_account: report.map(|r| r.missing_account),
            refused: report.map(|r| {
                let mut counts = BTreeMap::new();
                for reason in &r.refused {
                    *counts.entry(*reason).or_insert(0) += 1;
                }
                counts
            }),
        },
    };
    tx.execute("INSERT INTO inbox_status VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET history=excluded.history", params![serde_json::to_string(&history)?])?;
    tx.commit()?;
    #[cfg(unix)]
    std::fs::File::open(root)?.sync_all()?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct InboxView {
    pub name: String,
    pub waiting: Option<usize>,
    pub oldest_age_secs: Option<u64>,
    pub backend_state: &'static str,
    pub history_state: &'static str,
    pub last_successful_pull: Option<u64>,
    pub last_pull: Option<LastPull>,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub state: &'static str,
    pub inboxes: Vec<InboxView>,
}
impl Report {
    pub fn incomplete(&self) -> bool {
        self.state == "unknown"
            || self.inboxes.iter().any(|i| {
                i.backend_state == "unknown"
                    || i.history_state == "unknown"
                    || (i.waiting.is_some_and(|count| count > 0) && i.oldest_age_secs.is_none())
            })
    }
    pub fn print(&self) {
        if self.state == "unknown" {
            eprintln!("[remote inboxes] declarations unknown");
        }
        for i in &self.inboxes {
            let count = i
                .waiting
                .map_or_else(|| "unknown".into(), |v| v.to_string());
            let oldest = i.oldest_age_secs.map_or_else(
                || {
                    if i.waiting == Some(0) {
                        "not applicable".into()
                    } else {
                        "unknown".into()
                    }
                },
                |v| format!("{v}s"),
            );
            let last = i.last_successful_pull.map_or_else(
                || {
                    if i.history_state == "unknown" {
                        "unknown".into()
                    } else {
                        "not recorded".into()
                    }
                },
                |v| v.to_string(),
            );
            eprintln!("[remote inbox {}] waiting={count}; oldest_age={oldest}; last_successful_pull={last}; history={}", i.name, i.history_state);
            if let Some(pull) = &i.last_pull {
                let missing = pull
                    .missing_account
                    .map_or_else(|| "unknown".into(), |v| v.to_string());
                eprintln!(
                    "  last pull: state={}; exit_code={}; missing_account={missing}",
                    pull.state.label(),
                    pull.exit_code
                );
                if let Some(reasons) = &pull.refused {
                    for (reason, count) in reasons {
                        eprintln!("  refused: {reason:?}={count}");
                    }
                }
            }
        }
    }
}

/// Filesystem-only observation; unsupported backends stay unknown without a
/// network probe. Opaque object names cannot disclose a platform before pull.
fn waiting(locator: &str, now: u64) -> anyhow::Result<(usize, Option<u64>)> {
    let backend = locator
        .strip_prefix("fs://")
        .map(Path::new)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| anyhow::anyhow!("inbox backend observation unavailable"))?;
    safe(backend, true)?;
    let mut count = 0;
    let mut oldest = None;
    let mut age_unknown = false;
    for entry in std::fs::read_dir(backend)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "inbox object metadata unknown"
        );
        if metadata.is_dir() {
            continue;
        }
        anyhow::ensure!(metadata.is_file(), "inbox object metadata unknown");
        count += 1;
        match metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .and_then(|t| now.checked_sub(t))
        {
            Some(age) => oldest = Some(oldest.map_or(age, |previous: u64| previous.max(age))),
            None => age_unknown = true,
        }
    }
    Ok((count, if age_unknown { None } else { oldest }))
}

pub fn inspect(root: &Path, now: u64) -> Report {
    let mut report = Report {
        state: "known",
        inboxes: Vec::new(),
    };
    match std::fs::symlink_metadata(root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return report,
        _ => {
            if safe(root, true).is_err() {
                report.state = "unknown";
                return report;
            }
        }
    }
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => {
            report.state = "unknown";
            return report;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                report.state = "unknown";
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| inbox_config::validate(s, "memory://validation").is_ok())
        else {
            report.state = "unknown";
            continue;
        };
        let mut view = InboxView {
            name: name.into(),
            waiting: None,
            oldest_age_secs: None,
            backend_state: "unknown",
            history_state: "unknown",
            last_successful_pull: None,
            last_pull: None,
        };
        if let Ok(Some(config)) = inbox_config::load(root, name) {
            if let Ok((count, oldest)) = waiting(&config.locator, now) {
                view.waiting = Some(count);
                view.oldest_age_secs = oldest;
                view.backend_state = "known";
            }
        }
        match load(root, name) {
            Ok(Some(history)) => {
                view.history_state = "known";
                view.last_successful_pull = history.last_successful_pull;
                view.last_pull = Some(history.last_pull);
            }
            Ok(None) => view.history_state = "not_recorded",
            Err(_) => {}
        }
        report.inboxes.push(view);
    }
    report.inboxes.sort_by(|a, b| a.name.cmp(&b.name));
    report
}

pub fn inspect_default() -> Report {
    match u64::try_from(chrono::Utc::now().timestamp()) {
        Ok(now) => inspect(&inbox_config::default_root(), now),
        Err(_) => Report {
            state: "unknown",
            inboxes: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_retains_success_and_refusals_per_inbox_and_rejects_clock_regression() {
        let root = tempfile::tempdir().unwrap();
        assert!(load(root.path(), "synthetic-a").unwrap().is_none());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        record(
            root.path(),
            "synthetic-a",
            100,
            Some(&PullReport::default()),
        )
        .unwrap();
        let refused = PullReport {
            waiting: 2,
            refused: vec![Refusal::RevokedKey, Refusal::RevokedKey],
            ..PullReport::default()
        };
        record(root.path(), "synthetic-a", 101, Some(&refused)).unwrap();
        let history = load(root.path(), "synthetic-a").unwrap().unwrap();
        assert_eq!(history.last_successful_pull, Some(100));
        assert_eq!(history.last_pull.refused.unwrap()[&Refusal::RevokedKey], 2);
        assert_eq!(history.last_pull.exit_code, 1);
        record(root.path(), "synthetic-b", 102, None).unwrap();
        assert!(load(root.path(), "synthetic-b")
            .unwrap()
            .unwrap()
            .last_successful_pull
            .is_none());
        assert!(record(root.path(), "synthetic-a", 99, None).is_err());
        record(root.path(), "synthetic-a", 103, None).unwrap();
        let unknown = load(root.path(), "synthetic-a").unwrap().unwrap();
        assert_eq!(unknown.last_successful_pull, Some(100));
        assert!(unknown.last_pull.listed.is_none());
        assert!(unknown.last_pull.refused.is_none());
    }

    #[test]
    fn corrupt_or_incomplete_history_is_preserved_not_reinitialized() {
        let root = tempfile::tempdir().unwrap();
        record(
            root.path(),
            "synthetic-a",
            100,
            Some(&PullReport::default()),
        )
        .unwrap();
        let path = history_path(root.path(), "synthetic-a").unwrap();
        Connection::open(&path)
            .unwrap()
            .execute("DELETE FROM inbox_status", [])
            .unwrap();
        assert!(load(root.path(), "synthetic-a").is_err());
        assert!(record(root.path(), "synthetic-a", 101, None).is_err());
        std::fs::write(&path, b"synthetic-corruption").unwrap();
        assert!(load(root.path(), "synthetic-a").is_err());
        assert!(record(root.path(), "synthetic-a", 101, None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"synthetic-corruption");
        assert!(record(&root.path().join("missing"), "synthetic-a", 100, None).is_err());
        assert!(!root.path().join("missing").exists());
    }

    #[test]
    fn current_waiting_and_oldest_include_invalid_names_but_not_directories() {
        let root = tempfile::tempdir().unwrap();
        let backend = root.path().join("synthetic-backend");
        std::fs::create_dir(&backend).unwrap();
        let locator = format!("fs://{}", backend.display());
        assert_eq!(waiting(&locator, 100).unwrap(), (0, None));
        for (name, modified) in [
            ("synthetic-untrusted-name", 90),
            ("a".repeat(64).as_str(), 95),
        ] {
            let path = backend.join(name);
            std::fs::write(&path, b"synthetic-ciphertext").unwrap();
            std::fs::File::open(path)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new().set_modified(
                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(modified),
                    ),
                )
                .unwrap();
        }
        std::fs::create_dir(backend.join("synthetic-directory")).unwrap();
        assert_eq!(waiting(&locator, 100).unwrap(), (2, Some(10)));
        assert_eq!(waiting(&locator, 80).unwrap(), (2, None));
        let configs = root.path().join("synthetic-configs");
        inbox_config::initialize(&configs, "synthetic-a", &locator).unwrap();
        assert!(!inspect(&configs, 100).incomplete());
        assert!(inspect(&configs, 80).incomplete());
        std::fs::remove_dir_all(&backend).unwrap();
        assert!(waiting(&locator, 100).is_err());
        assert!(!backend.exists());
        assert!(waiting("s3://synthetic-unreachable", 100).is_err());
    }

    #[test]
    fn observation_refuses_unsafe_declarations_and_history_on_every_platform() {
        let root = tempfile::tempdir().unwrap();
        let configs = root.path().join("synthetic-configs");
        inbox_config::initialize(&configs, "synthetic-a", "s3://synthetic-unreachable").unwrap();
        let view = inspect(&configs, 100);
        assert!(view.incomplete());
        assert!(view.inboxes[0].waiting.is_none());
        assert_eq!(view.inboxes[0].history_state, "not_recorded");
        let path = history_path(&configs, "synthetic-a").unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(record(&configs, "synthetic-a", 100, None).is_err());
        assert!(load(&configs, "synthetic-a").is_err());
        #[cfg(unix)]
        {
            std::fs::remove_dir(&path).unwrap();
            let outside = root.path().join("synthetic-outside");
            std::fs::write(&outside, b"synthetic-protected").unwrap();
            std::os::unix::fs::symlink(&outside, &path).unwrap();
            assert!(record(&configs, "synthetic-a", 100, None).is_err());
            assert!(load(&configs, "synthetic-a").is_err());
            assert_eq!(std::fs::read(outside).unwrap(), b"synthetic-protected");
        }
        // Windows symlinks require privileges; the non-file boundary is tested
        // above without requiring a privileged fixture on that platform.
    }
}
