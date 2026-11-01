//! W1054 — SG-2d: the SG-2b rule ("after reclaim, change detection does not
//! use the stage body as its baseline") for opencode's SQLite sessions.
//!
//! The gap SG-2b left: an opencode entry written before ADR-061 carries no
//! record digests, so it endorses no increment map and its session is
//! observed whole whenever its cursor moves. With the stage body present
//! that was a cost; once reclaim has removed the body it must not become a
//! second one — the 11-01 run after the first big reclaim re-exported 727
//! such sessions (reset=true) for a handful of new records each. The
//! durable map under `meta/<machine>` (SG-2b) is the baseline instead, once
//! the retained manifest summary shows the body was reclaimed (not lost).
//!
//! Required fixtures, all through the real `reclaim_stage --apply` (pre-pass
//! included) on a throwaway local repository:
//!
//! 1. A reclaimed pre-increment session whose source did not change is NOT
//!    re-exported (no shard, no lines, no reset) — the acceptance.
//! 2. The same session after it gained one message + part writes exactly that
//!    increment (session row + 1 message + 1 part), never the whole session,
//!    and the archive's assembled view is byte-identical to a fresh export.
//!    Afterwards the entry is in the increment era: the next pass is silent.
//! 3. Unknown is never "nothing new": with the durable map gone, the same
//!    grown session degrades to exactly one complete observation.
//! 4. Destination scoping (H1): a destination that never pushed the session
//!    still receives it in full; the machine-level baseline does not speak
//!    for it.
//!
//! Everything runs in `tempfile` scratch directories against a synthetic
//! SQLite store; no real harness store is read, no network is touched, and
//! assertions stay metadata-shaped (counts, bytes, synthetic record ids).

use chat_stasher::collect::{self, CollectReport, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::destdiag::DestinationHealth;
use chat_stasher::scanner::{self, HarnessRegistry, ScanReport};
use chat_stasher::sqlite_probe;
use chat_stasher::stagereclaim::{self, NamedStore};
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rusqlite::Connection;
use rustic_core::repofile::MasterKey;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const MACHINE: &str = "fixture-machine";
const DESTINATION: &str = "fixture-destination";
const SESSION: &str = "ses_w1054";
const OTHER_SESSION: &str = "ses_w1054_other";

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

fn store_config(dir: &Path, repo_name: &str) -> StoreConfig {
    StoreConfig {
        repo_root: dir.join(repo_name).to_string_lossy().into_owned(),
        key_file: dir.join(format!("{repo_name}.key")),
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
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("opencode.db");
        create_store(&db);
        let stage = dir.path().join("stage");
        let state = dir.path().join("state");
        let fx = Fixture {
            dir,
            db,
            stage,
            state,
        };
        create_base_session(&fx.db);
        fx
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

    fn collect_for(&self, destination: &str) -> CollectReport {
        let dest = DestinationView::unreachable(destination);
        collect::collect_scan_report(&self.scan(), &self.stage, MACHINE, &self.state, 20, &dest)
            .unwrap()
    }

    fn collect(&self) -> CollectReport {
        self.collect_for(DESTINATION)
    }

    fn session_id(&self) -> String {
        self.scan()
            .records
            .iter()
            .map(|record| record.id.clone())
            .find(|id| id.ends_with(&format!(".{SESSION}")))
            .unwrap_or_else(|| panic!("scan produced no record for {SESSION}"))
    }

    fn stage_shard_bodies(&self, session_id: &str) -> Vec<Vec<u8>> {
        let dir = store::session_shard_dir(&self.stage, MACHINE, session_id);
        let mut entries = store::sealed_shard_entries(&dir).unwrap_or_default();
        entries.sort_by_key(|(seq, _)| *seq);
        entries
            .into_iter()
            .map(|(_, path)| fs::read(&path).unwrap())
            .collect()
    }

    fn meta_map(&self, session_id: &str) -> PathBuf {
        collect::opencode_record_map_meta_path(&self.stage, MACHINE, session_id).unwrap()
    }

    /// The source gains one message with one part (and the session row's
    /// `time_updated` moves, as opencode does on every turn).
    fn grow(&self) {
        let conn = connect(&self.db);
        conn.execute(
            "UPDATE session SET time_updated = 3000 WHERE id = ?1",
            [SESSION],
        )
        .unwrap();
        add_message(&conn, 2600, "user", "m3");
        add_part(
            &conn,
            "m3",
            "p4",
            2700,
            r#"{"type":"text","text":"synthetic four"}"#,
        );
    }

    /// Some other session in the same store is written: the store-wide
    /// fingerprint moves, so every session falls through to its own
    /// per-session comparison (opencode's normal weekday).
    fn touch_other_session(&self) {
        let conn = connect(&self.db);
        conn.execute(
            "INSERT OR REPLACE INTO session (id, time_created, time_updated, title)
             VALUES (?1, 1, 2, 'other')",
            [OTHER_SESSION],
        )
        .unwrap();
    }
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

fn create_base_session(db: &Path) {
    let conn = connect(db);
    conn.execute(
        "INSERT INTO session (id, time_created, time_updated, title)
         VALUES (?1, 1000, 1000, 'fixture')",
        [SESSION],
    )
    .unwrap();
    add_message(&conn, 2000, "user", "m1");
    add_part(
        &conn,
        "m1",
        "p1",
        2100,
        r#"{"type":"text","text":"synthetic one"}"#,
    );
    add_message(&conn, 2200, "assistant", "m2");
    add_part(
        &conn,
        "m2",
        "p2",
        2300,
        r#"{"type":"text","text":"synthetic two"}"#,
    );
}

fn add_message(conn: &Connection, time_created: i64, role: &str, mid: &str) {
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?3, ?4)",
        rusqlite::params![
            format!("{SESSION}-{mid}"),
            SESSION,
            time_created,
            json!({"role": role}).to_string()
        ],
    )
    .unwrap();
}

fn add_part(conn: &Connection, mid: &str, pid: &str, time_created: i64, data: &str) {
    conn.execute(
        "INSERT INTO part (id, session_id, message_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        rusqlite::params![
            format!("{SESSION}-{pid}"),
            SESSION,
            format!("{SESSION}-{mid}"),
            time_created,
            data
        ],
    )
    .unwrap();
}

fn fresh_export_framed(fx: &Fixture) -> Vec<u8> {
    let snapshot = sqlite_probe::read_opencode_session(&fx.db, SESSION).unwrap();
    let mut framed = snapshot.json_line;
    framed.push(b'\n');
    framed
}

fn shard_lines(body: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(body)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap_or(Value::Null))
        .collect()
}

/// Remove every key `name` from a JSON tree.
fn strip_key(value: &mut Value, name: &str) {
    match value {
        Value::Object(map) => {
            map.remove(name);
            for inner in map.values_mut() {
                strip_key(inner, name);
            }
        }
        Value::Array(items) => {
            for inner in items {
                strip_key(inner, name);
            }
        }
        _ => {}
    }
}

/// Drop the state directory's copy of the increment maps (it may not exist
/// yet; a missing directory is the state this wants).
fn forget_state_maps(fx: &Fixture) {
    match fs::remove_dir_all(fx.state.join("opencode-session-maps")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("cannot clear the state maps: {e}"),
    }
}

/// Make the destination's stored entries look like the ones written before
/// ADR-061: no record digests, so no entry endorses an increment map.
fn make_entries_pre_increment(fx: &Fixture) {
    let path = fx.state.join("debts-v2.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    strip_key(&mut state, "opencode_record_digests");
    strip_key(&mut state, "opencode_map_digest");
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
}

/// Reproduce a stage reclaimed by the real `reclaim --apply` (pre-pass,
/// summary refresh, body deletion) over an entry that predates ADR-061:
/// the only maps that exist afterwards are the ones the pre-pass folded
/// from the staged body.
///
/// `history` first grows the session once and collects again, so the staged
/// body is two observations. Returns the session id, the pre-reclaim
/// assembled bytes and the repository key.
fn reclaimed_pre_increment_session(fx: &Fixture, history: bool) -> (String, Vec<u8>, MasterKey) {
    fx.collect();
    if history {
        // A stage holding two observations: the manifest's summary of it then
        // equals no single export, so it cannot vouch for the current one.
        fx.grow();
        fx.collect();
    }
    let id = fx.session_id();
    let cfg = store_config(fx.dir.path(), "repo-a");
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&fx.stage, &mk)
        .unwrap();
    let (pre_reclaim, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &id, &mk)
        .unwrap();

    make_entries_pre_increment(fx);
    // No map survives from before: the pre-pass must synthesize it.
    forget_state_maps(fx);
    fs::remove_file(fx.meta_map(&id)).unwrap();
    assert!(!fx.meta_map(&id).exists());

    let dests = vec![NamedStore {
        name: "repo-a".to_string(),
        cfg: cfg.clone(),
        health: DestinationHealth::Healthy,
    }];
    let report = stagereclaim::reclaim_stage(&fx.stage, &dests, true, None).unwrap();
    assert!(!report.blocked(), "reclaim blocked: {:?}", report.blocked);
    assert_eq!(report.reclaimed.len(), 1);
    assert!(
        fx.stage_shard_bodies(&id).is_empty(),
        "the body must be reclaimed off the stage"
    );
    assert!(
        fx.meta_map(&id).exists(),
        "the pre-pass must persist the map"
    );
    (id, pre_reclaim, mk)
}

/// An entry the unchanged-export guard cannot vouch for: the export hash it
/// recorded is blank and its cursor predates the current one (an older
/// cursor shape), so the pass reads the session and must decide for itself
/// whether the archive already holds what it just read.
fn make_entries_unvouchable(fx: &Fixture) {
    let path = fx.state.join("debts-v2.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    fn walk(value: &mut Value) {
        match value {
            Value::Object(map) => {
                if map.contains_key("prefix_sha256") && map.contains_key("opencode") {
                    map.insert("prefix_sha256".to_string(), json!(""));
                    if let Some(Value::Object(cursor)) = map.get_mut("opencode") {
                        cursor.insert("session_time_updated".to_string(), json!(1));
                    }
                }
                for inner in map.values_mut() {
                    walk(inner);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut state);
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
}

fn outcome_of<'a>(report: &'a CollectReport, session_id: &str) -> &'a collect::CollectOutcome {
    report
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(session_id))
        .expect("session must be in the pass outcomes")
}

/// Acceptance: a session reclaimed off the stage is NOT re-exported in the
/// next run.
#[test]
fn a_reclaimed_pre_increment_session_is_not_reexported() {
    let fx = Fixture::new();
    let (id, _, _) = reclaimed_pre_increment_session(&fx, false);
    fx.touch_other_session();

    let next = fx.collect();

    let outcome = outcome_of(&next, &id);
    assert_eq!(
        outcome.lines_written, 0,
        "an unchanged session writes nothing"
    );
    assert!(outcome.shard.is_none());
    assert!(!outcome.reset, "an unchanged session is not a reset");
    assert!(
        fx.stage_shard_bodies(&id).is_empty(),
        "nothing may come back onto the stage"
    );
    assert_eq!(next.unverified_cursors, 0);
    assert_eq!(
        next.reset_records, 0,
        "the pass must report no reset for the reclaimed session"
    );
}

/// The same acceptance when the entry cannot vouch for the export it last
/// recorded: before the fix this was a whole-session re-export of a session
/// whose bytes the archive already held.
#[test]
fn a_reclaimed_session_whose_entry_cannot_vouch_for_its_export_is_not_reexported() {
    let fx = Fixture::new();
    let (id, _, _) = reclaimed_pre_increment_session(&fx, true);
    make_entries_unvouchable(&fx);
    fx.touch_other_session();

    let next = fx.collect();

    let outcome = outcome_of(&next, &id);
    assert_eq!(outcome.lines_written, 0);
    assert!(outcome.shard.is_none());
    assert!(!outcome.reset);
    assert!(fx.stage_shard_bodies(&id).is_empty());
    assert_eq!(next.reset_records, 0);
}

/// The session grew after reclaim: only the increment is written, the
/// assembled view equals a fresh export, and the entry is then in the
/// increment era (the following pass is silent).
#[test]
fn a_reclaimed_pre_increment_session_that_grew_writes_the_increment_only() {
    let fx = Fixture::new();
    let (id, _, key_mk) = reclaimed_pre_increment_session(&fx, false);
    fx.grow();

    let next = fx.collect();

    let outcome = outcome_of(&next, &id);
    assert!(
        outcome.shard.is_some() && outcome.lines_written > 0,
        "a changed session must write (if treated as unchanged, this fails)"
    );
    let bodies = fx.stage_shard_bodies(&id);
    assert_eq!(bodies.len(), 1, "one increment shard, not a second copy");
    let lines = shard_lines(&bodies[0]);
    assert_eq!(
        lines.len(),
        3,
        "session row + 1 message + 1 part, not the whole session"
    );
    for new_id in ["m3", "p4"] {
        assert!(lines
            .iter()
            .any(|l| l["record_id"] == format!("{SESSION}-{new_id}")));
    }
    for old_id in ["m1", "m2", "p1", "p2"] {
        assert!(
            !lines
                .iter()
                .any(|l| l["record_id"] == format!("{SESSION}-{old_id}")),
            "record {old_id} was delivered before the reclaim and must not be restaged"
        );
    }
    // Push and read back: the assembled view is the source's final state.
    let cfg = store_config(fx.dir.path(), "repo-a");
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&fx.stage, &key_mk)
        .unwrap();
    let (assembled, _) = BackupStore::new(cfg, MACHINE.to_string())
        .read_session_concat(MACHINE, &id, &key_mk)
        .unwrap();
    assert_eq!(assembled, fresh_export_framed(&fx));

    // Converted to the increment era: the next pass writes nothing.
    fx.touch_other_session();
    let again = fx.collect();
    // (the other session is first sighted here, so only this session's own
    // outcome speaks for it)
    let outcome = outcome_of(&again, &id);
    assert!(outcome.shard.is_none());
    assert_eq!(outcome.lines_written, 0);
    assert!(!outcome.reset);
    assert_eq!(fx.stage_shard_bodies(&id).len(), 1);
}

/// Unknown is never "nothing new": with no durable map the grown session is
/// observed whole, once.
#[test]
fn without_a_durable_map_a_grown_reclaimed_session_degrades_to_one_full_observation() {
    let fx = Fixture::new();
    let (id, _, _) = reclaimed_pre_increment_session(&fx, false);
    fs::remove_file(fx.meta_map(&id)).unwrap();
    forget_state_maps(&fx);
    fx.grow();

    let next = fx.collect();

    let outcome = outcome_of(&next, &id);
    assert_eq!(outcome.lines_written, 1, "one complete observation");
    let bodies = fx.stage_shard_bodies(&id);
    assert_eq!(bodies.len(), 1);
    let mut whole = bodies[0].clone();
    if whole.last() != Some(&b'\n') {
        whole.push(b'\n');
    }
    assert_eq!(whole, fresh_export_framed(&fx));
}

/// H1: the baseline is machine-level; a destination that never pushed the
/// session is still handed it in full.
#[test]
fn a_destination_that_never_pushed_the_session_still_receives_it_in_full() {
    let fx = Fixture::new();
    let (id, _, _) = reclaimed_pre_increment_session(&fx, false);

    let next = fx.collect_for("a-destination-that-never-pushed");

    let outcome = outcome_of(&next, &id);
    assert_eq!(
        outcome.lines_written, 1,
        "the full body, for the new destination"
    );
    let bodies = fx.stage_shard_bodies(&id);
    assert_eq!(bodies.len(), 1);
    let mut whole = bodies[0].clone();
    if whole.last() != Some(&b'\n') {
        whole.push(b'\n');
    }
    assert_eq!(whole, fresh_export_framed(&fx));
}
