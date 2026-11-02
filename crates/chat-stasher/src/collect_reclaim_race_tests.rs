//! W1057 — the 11-01 "727 opencode resets": a collect pass in flight while
//! the weekly `reclaim-stage` finishes.
//!
//! `reclaim-stage` and a collect pass take no lock against each other, and
//! the hourly pass runs for most of the hour. A pass reads the retained
//! manifest once, at its start. A reclaim that finishes while the pass is
//! mid-flight writes the new summary first and then deletes the bodies; the
//! sessions the pass has not reached yet are then bodiless, and the summary
//! that settles them is not the one this pass holds. Their (healthy,
//! digested, unchanged) cursors read as unverifiable and each session was
//! reread whole.
//!
//! Production shape reproduced here, all through the real
//! `reclaim_stage(apply)`:
//!
//! * the store holds an earlier-reclaimed session, so a manifest exists but
//!   does not name the sessions below;
//! * two increment-era sessions (digests on their entries, two shards each)
//!   that did not change since their last pass;
//! * the reclaim finishes between two records of one pass.
//!
//! The fixtures are synthetic SQLite stores in `tempfile` directories;
//! assertions are counts.

use crate::collect::{self, tests_support, CollectReport, DestinationView};
use crate::config::Config;
use crate::destdiag::DestinationHealth;
use crate::resetledger;
use crate::scanner::{self, HarnessRegistry, ScanReport};
use crate::stagereclaim::{self, NamedStore};
use crate::store::{self, BackupStore, StoreConfig};
use rusqlite::Connection;
use rustic_core::repofile::MasterKey;
use serde_json::json;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const MACHINE: &str = "fixture-machine";
const DESTINATION: &str = "fixture-destination";

fn sqlite_registry() -> HarnessRegistry {
    let cell = json!({
        "template": "$XDG_DATA_HOME/opencode/opencode.db",
        "format": "sqlite",
        "confidence": "source-confirmed",
        "source": "synthetic fixture"
    });
    let paths = match scanner::current_platform() {
        "macos" => json!({"macos": cell}),
        "linux" => json!({"linux": cell}),
        "windows" => json!({"windows": cell}),
        platform => panic!("unexpected platform: {platform}"),
    };
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic",
        "harnesses": [{
            "id": "opencode",
            "display_name": "synthetic",
            "paths": paths
        }]
    }))
    .unwrap()
}

fn connect(db: &Path) -> Connection {
    Connection::open(db).unwrap()
}

fn create_store(db: &Path) {
    connect(db)
        .execute_batch(
            "CREATE TABLE session (
                 id TEXT PRIMARY KEY,
                 time_created INTEGER NOT NULL,
                 time_updated INTEGER NOT NULL,
                 title TEXT
             );
             CREATE TABLE message (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL,
                 time_updated INTEGER NOT NULL,
                 data TEXT NOT NULL
             );
             CREATE TABLE part (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL,
                 time_updated INTEGER NOT NULL,
                 data TEXT NOT NULL
             );",
        )
        .unwrap();
}

fn add_session(db: &Path, session: &str) {
    let conn = connect(db);
    conn.execute(
        "INSERT INTO session (id, time_created, time_updated, title)
         VALUES (?1, 1000, 1000, 'fixture')",
        [session],
    )
    .unwrap();
    add_turn(db, session, "m1", 2000);
}

/// One more message with one part; moves the session's `time_updated`.
fn add_turn(db: &Path, session: &str, mid: &str, at: i64) {
    let conn = connect(db);
    conn.execute(
        "UPDATE session SET time_updated = ?2 WHERE id = ?1",
        rusqlite::params![session, at],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?3, ?4)",
        rusqlite::params![
            format!("{session}-{mid}"),
            session,
            at,
            json!({"role": "user"}).to_string()
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, session_id, message_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        rusqlite::params![
            format!("{session}-{mid}-p"),
            session,
            format!("{session}-{mid}"),
            at + 1,
            r#"{"type":"text","text":"synthetic"}"#
        ],
    )
    .unwrap();
}

/// Move the store-wide fingerprint without touching any session, the way
/// another session being written does on a real weekday.
fn touch_store(db: &Path) {
    connect(db)
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS noise (x INTEGER); INSERT INTO noise VALUES (1);",
        )
        .unwrap();
}

fn store_config(dir: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: dir.join("repo-a").to_string_lossy().into_owned(),
        key_file: dir.join("repo-a.key"),
        connections: 1,
        options: BTreeMap::new(),
        // Tests must not share the user's rustic cache (W289).
        cache_dir: None,
        no_cache: true,
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    db: PathBuf,
    stage: PathBuf,
    state: PathBuf,
    cfg: StoreConfig,
    mk: MasterKey,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("opencode.db");
        create_store(&db);
        let cfg = store_config(dir.path());
        let mk = MasterKey::new();
        store::persist_key_file(&cfg, &mk).unwrap();
        Fixture {
            stage: dir.path().join("stage"),
            state: dir.path().join("state"),
            db,
            cfg,
            mk,
            dir,
        }
    }

    fn scan(&self) -> ScanReport {
        let mut roots = BTreeMap::new();
        roots.insert(
            "opencode".to_string(),
            self.db.to_string_lossy().into_owned(),
        );
        let config = Config {
            harness_roots: roots,
            ..Config::default()
        };
        scanner::scan_with_registry(&config, &sqlite_registry()).unwrap()
    }

    fn collect(&self) -> CollectReport {
        let dest = DestinationView::unreachable(DESTINATION);
        collect::collect_scan_report(&self.scan(), &self.stage, MACHINE, &self.state, 20, &dest)
            .unwrap()
    }

    fn push(&self) {
        BackupStore::new(self.cfg.clone(), MACHINE.to_string())
            .push(&self.stage, &self.mk)
            .unwrap();
    }

    fn reclaim(&self) {
        reclaim_with(&self.stage, &self.cfg);
    }

    fn bodies_left(&self) -> usize {
        let Ok(dir) = std::fs::read_dir(self.stage.join("sessions").join(MACHINE)) else {
            return 0;
        };
        dir.flatten()
            .filter(|entry| match store::sealed_shard_entries(&entry.path()) {
                Ok(shards) => !shards.is_empty(),
                Err(_) => false,
            })
            .count()
    }
}

fn reclaim_with(stage: &Path, cfg: &StoreConfig) {
    let dests = vec![NamedStore {
        name: "repo-a".to_string(),
        cfg: cfg.clone(),
        health: DestinationHealth::Healthy,
    }];
    let report = stagereclaim::reclaim_stage(stage, &dests, true, None).unwrap();
    assert!(!report.blocked(), "reclaim blocked: {:?}", report.blocked);
}

/// The state production was in just before the 11-01 reclaim finished: an
/// earlier reclaim left a manifest that does not name the two sessions
/// below, and those two are increment-era (a first export, then a record
/// increment: digests on the entry, two shards on the stage), pushed, and
/// unchanged since their last pass.
fn pre_reclaim_production_shape() -> Fixture {
    let fx = Fixture::new();
    add_session(&fx.db, "ses_old");
    fx.collect();
    fx.push();
    fx.reclaim();
    assert!(
        fx.bodies_left() == 0,
        "the earlier reclaim emptied the stage"
    );
    assert!(
        crate::manifest::manifest_path(&fx.stage, MACHINE).is_file(),
        "an earlier reclaim leaves a manifest"
    );

    add_session(&fx.db, "ses_a");
    add_session(&fx.db, "ses_b");
    fx.collect();
    add_turn(&fx.db, "ses_a", "m2", 3000);
    add_turn(&fx.db, "ses_b", "m2", 3000);
    let growth = fx.collect();
    assert!(
        growth.reset_records >= 2,
        "both sessions were written by an increment pass"
    );
    fx.push();
    assert_eq!(fx.bodies_left(), 2);
    let quiet = fx.collect();
    assert_eq!((quiet.reset_records, quiet.shards_written), (0, 0));
    touch_store(&fx.db);
    fx
}

/// The 11-01 shape. RED without the manifest re-read: the session(s) the
/// pass reaches after the reclaim finished are judged unverifiable and
/// reread whole.
#[test]
fn a_reclaim_finishing_mid_pass_does_not_reset_the_sessions_the_pass_reaches_after_it() {
    let fx = pre_reclaim_production_shape();

    let reached = Rc::new(Cell::new(0_usize));
    let hook_reached = Rc::clone(&reached);
    let (stage, cfg) = (fx.stage.clone(), fx.cfg.clone());
    tests_support::set_before_record(Some(Box::new(move |_| {
        // The reclaim finishes before the pass reads its second record.
        if hook_reached.get() == 1 {
            reclaim_with(&stage, &cfg);
        }
        hook_reached.set(hook_reached.get() + 1);
    })));
    let report = fx.collect();
    tests_support::set_before_record(None);

    assert_eq!(reached.get(), 3, "the pass reached all three records");
    assert_eq!(fx.bodies_left(), 0, "the reclaim finished inside the pass");
    assert_eq!(
        report.unverified_cursors, 0,
        "healthy, unchanged cursors reclaimed under the pass are settled by the manifest the reclaim wrote"
    );
    assert_eq!(report.reset_records, 0, "causes: {:?}", report.reset_causes);
    assert_eq!(report.shards_written, 0);
    assert!(
        report.late_manifest_settled >= 1,
        "the re-read manifest settled at least one session"
    );
    assert!(report.manifest_reloads >= 1);

    // And the round after it is as quiet as any other.
    touch_store(&fx.db);
    let next = fx.collect();
    assert_eq!((next.reset_records, next.shards_written), (0, 0));
}

/// Control: the same reclaim finishing BEFORE the pass starts is silent
/// (this is the sequential shape W1054's fixtures covered), so the race is
/// what the fixture above adds.
#[test]
fn a_reclaim_that_finished_before_the_pass_began_is_silent() {
    let fx = pre_reclaim_production_shape();
    fx.reclaim();
    assert_eq!(fx.bodies_left(), 0);
    let rows_before = resetledger::read_rounds(&fx.state).unwrap().len();
    let report = fx.collect();
    assert_eq!(report.unverified_cursors, 0);
    assert_eq!(report.reset_records, 0);
    assert_eq!(report.shards_written, 0);
    assert_eq!(report.late_manifest_settled, 0);
    assert_eq!(
        resetledger::read_rounds(&fx.state).unwrap().len(),
        rows_before,
        "a quiet round leaves the state directory as it found it"
    );
}

/// Diagnostics: a reset that really is forced says why, in counts, per
/// source, and the round survives in the ledger beside the state files.
#[test]
fn every_reset_is_counted_by_cause_and_the_round_stays_in_the_ledger() {
    let fx = pre_reclaim_production_shape();
    fx.reclaim();
    // The retained summary is lost: nothing local can settle the bodiless
    // sessions, and no archive is consulted.
    std::fs::remove_file(crate::manifest::manifest_path(&fx.stage, MACHINE)).unwrap();
    let report = fx.collect();
    let forced = BTreeMap::from([(
        (
            "opencode".to_string(),
            "unverifiable:manifest_absent+archive_unconsulted".to_string(),
        ),
        3_usize,
    )]);
    assert_eq!(report.unverified_cursors, 3);
    assert_eq!(report.reset_records, 3);
    assert_eq!(report.reset_causes, forced);
    assert_eq!(report.manifest_rows_at_start, None);

    // A normal record increment is a counted reset too, under its own word.
    add_turn(&fx.db, "ses_a", "m3", 4000);
    let grown = fx.collect();
    assert_eq!(grown.reset_records, 1);
    assert_eq!(
        grown.reset_causes,
        BTreeMap::from([(
            (
                "opencode".to_string(),
                "opencode_increment_write".to_string()
            ),
            1_usize
        )])
    );

    // The ledger holds every round, the oldest first, with no session text.
    let rows = resetledger::read_rounds(&fx.state).unwrap();
    assert!(rows.len() >= 2);
    let before_last = &rows[rows.len() - 2];
    assert_eq!(before_last.reset, 3);
    assert_eq!(before_last.unverified_cursors, 3);
    assert_eq!(
        before_last.reset_causes["opencode"]["unverifiable:manifest_absent+archive_unconsulted"],
        3
    );
    let last = rows.last().unwrap();
    assert_eq!(last.reset_causes["opencode"]["opencode_increment_write"], 1);
    assert!(rows.iter().take(rows.len() - 2).all(|row| row.reset <= 2));
    let raw = std::fs::read_to_string(resetledger::path(&fx.state)).unwrap();
    assert!(
        !raw.contains("ses_") && !raw.contains("synthetic"),
        "the ledger names no session"
    );
    assert_eq!(report.reset_ledger_error, None);
}
