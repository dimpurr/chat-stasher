//! W1054f2 — the SG-2d follow-up the W1054 verdict opened: why do digested,
//! unchanged, reclaimed opencode entries re-export, and what should the pass
//! do about it.
//!
//! The verdict's review pinned the production fact (11-01: 727 whole-session
//! re-exports, all increment-era, all digested) and left two candidate
//! branches; the follow-up established, from the retained state plus the
//! ledger, that the debt branch is the one that fired (a healthy, digested,
//! unchanged cursor read as unverifiable and force-reread whole) — the
//! increment map was intact where the re-exports happened, and W1057 keeps
//! the mid-pass reclaim race answered by re-reading the manifest. What the
//! race fix does not answer is the class these fixtures hold: a reclaimed
//! destination whose stored shard fact names a *subset* of what the reclaim
//! later proved and retained — its row covers the debt, but exact equality
//! turns that cover into "unverifiable", and an unconsultable archive then
//! rereads a session the archives provably hold, whole.
//!
//! Three shapes through the real `reclaim_stage(apply)`, per the verdict's
//! R1/R2/R3:
//!
//! * R1, control: increment-era, unchanged, reclaimed, whose fact equals the
//!   retained one — silent before and after (the healthy path, and the proof
//!   the new coverage rule changes nothing it did not owe).
//! * R2: increment-era whose recorded shard fact lags the stage body because
//!   a second destination's first sight sealed the growth, reclaimed, and a
//!   destination archive that cannot be consulted. Before the coverage rule
//!   this was `lines=1 reset=true unverified=1` — the whole re-export the
//!   verdict flagged; after it the retained summary settles the debt and the
//!   reclaim-proven durable map is the baseline, so the pass writes nothing.
//! * R3: increment-era, grown after reclaim, both durable map copies gone —
//!   unknown is never "nothing new": exactly one whole observation, by a
//!   named cause (`opencode_map_unreconciled`), never a forced reread, and
//!   the next pass is silent again.
//!
//! Everything runs in `tempfile` scratch directories against a synthetic
//! SQLite store; no real harness store is read, no network is touched, and
//! assertions stay metadata-shaped (counts, byte sizes, synthetic ids).

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
const SECOND_DESTINATION: &str = "fixture-second-destination";
const SESSION: &str = "ses_w1054f2";
const OTHER_SESSION: &str = "ses_w1054f2_other";

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
    cfg: StoreConfig,
    masterkey: MasterKey,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("opencode.db");
        let stage = dir.path().join("stage");
        let state = dir.path().join("state");
        let cfg = store_config(dir.path(), "repo-a");
        let masterkey = MasterKey::new();
        store::persist_key_file(&cfg, &masterkey).unwrap();
        connect(&db)
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
        connect(&db)
            .execute(
                "INSERT INTO session (id, time_created, time_updated, title)
                 VALUES (?1, 1000, 1000, 'fixture')",
                [SESSION],
            )
            .unwrap();
        add_turn(&db, SESSION, "m1", 2000);
        Fixture {
            dir,
            db,
            stage,
            state,
            cfg,
            masterkey,
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

    /// Collect for one destination whose archive cannot be consulted — the
    /// verdict's "destination archive not consultable", the state a degraded
    /// or unreachable destination leaves the collect in.
    fn collect_for(&self, destination: &str) -> CollectReport {
        let dest = DestinationView::unreachable(destination);
        collect::collect_scan_report(&self.scan(), &self.stage, MACHINE, &self.state, 20, &dest)
            .unwrap()
    }

    fn collect(&self) -> CollectReport {
        self.collect_for(DESTINATION)
    }

    fn push(&self) {
        BackupStore::new(self.cfg.clone(), MACHINE.to_string())
            .push(&self.stage, &self.masterkey)
            .unwrap();
    }

    fn reclaim(&self) {
        let dests = vec![NamedStore {
            name: "repo-a".to_string(),
            cfg: self.cfg.clone(),
            health: DestinationHealth::Healthy,
        }];
        let report = stagereclaim::reclaim_stage(&self.stage, &dests, true, None).unwrap();
        assert!(!report.blocked(), "reclaim blocked: {:?}", report.blocked);
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
        match store::sealed_shard_entries(&dir) {
            Ok(mut entries) => {
                entries.sort_by_key(|(seq, _)| *seq);
                entries
                    .into_iter()
                    .map(|(_, path)| fs::read(path).unwrap())
                    .collect()
            }
            Err(_) => Vec::new(),
        }
    }

    /// The destination's stored debt entry for this session, straight from the
    /// state file: `{ shards }`, or `None` when that destination was never
    /// read for this session.
    fn debt_shards_of(&self, destination: &str, session_id: &str) -> Option<Value> {
        let path = self.state.join("debts-v2.json");
        let state: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        state
            .get("destinations")?
            .get(destination)?
            .get("files")?
            .as_object()?
            .values()
            .find(|entry| entry.get("session_id").and_then(Value::as_str) == Some(session_id))
            .and_then(|entry| entry.get("shards"))
            .cloned()
    }

    /// This session's retained manifest row, from the stored summary the
    /// reclaim keeps.
    fn manifest_row(&self, session_id: &str) -> Option<Value> {
        let path = chat_stasher::manifest::manifest_path(&self.stage, MACHINE);
        for line in String::from_utf8_lossy(&fs::read(path).ok()?).lines() {
            if line.trim().is_empty() {
                continue;
            }
            let row: Value = serde_json::from_str(line).ok()?;
            if row.get("session_id").and_then(Value::as_str) == Some(session_id) {
                return Some(row);
            }
        }
        None
    }

    fn meta_map(&self, session_id: &str) -> PathBuf {
        collect::opencode_record_map_meta_path(&self.stage, MACHINE, session_id).unwrap()
    }

    /// Some other session in the same store is written: the store-wide
    /// fingerprint moves, so every session falls through to its own
    /// per-session comparison (opencode's normal weekday). Without this the
    /// pass would answer the target session from the store shortcut and
    /// the fixture would prove nothing about its debt or its map.
    fn touch_other_session(&self) {
        connect(&self.db)
            .execute(
                "INSERT OR REPLACE INTO session (id, time_created, time_updated, title)
                 VALUES (?1, 1, 2, 'other')",
                [OTHER_SESSION],
            )
            .unwrap();
    }

    /// Drop every durable copy of the increment map (meta and state), the way
    /// a stage the pre-pass never reached survived the 10-27 reclaim.
    fn forget_maps(&self, session_id: &str) {
        fs::remove_file(self.meta_map(session_id)).unwrap();
        match fs::remove_dir_all(self.state.join("opencode-session-maps")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("cannot clear the state maps: {e}"),
        }
        assert!(!self.meta_map(session_id).exists());
    }
}

fn connect(db: &Path) -> Connection {
    Connection::open(db).unwrap()
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
            format!("{session}-{mid}-p1"),
            session,
            format!("{session}-{mid}"),
            at + 1,
            r#"{"type":"text","text":"synthetic"}"#
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

fn outcome_of<'a>(report: &'a CollectReport, session_id: &str) -> &'a collect::CollectOutcome {
    report
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(session_id))
        .expect("session must be in the pass outcomes")
}

/// R1 — the control, which must stay silent before the coverage rule and
/// after it: an increment-era session, reclaimed, whose stored shard fact
/// the retained row equals, and unchanged since — the pass writes nothing,
/// stages nothing, and reports no reset and no unverified cursor.
#[test]
fn r1_an_unchanged_reclaimed_increment_era_session_stays_silent() {
    let fx = Fixture::new();
    let first = fx.collect();
    assert!(
        outcome_of(&first, &fx.session_id()).shard.is_some(),
        "setup: the first pass sealed the whole session"
    );
    fx.push();
    fx.reclaim();
    let id = fx.session_id();
    assert!(fx.stage_shard_bodies(&id).is_empty(), "setup: reclaimed");
    fx.touch_other_session();

    let next = fx.collect();

    let outcome = outcome_of(&next, &id);
    assert_eq!(outcome.lines_written, 0);
    assert!(outcome.shard.is_none());
    assert!(!outcome.reset);
    assert_eq!(next.unverified_cursors, 0);
    assert_eq!(next.reset_records, 0);
    assert!(
        fx.stage_shard_bodies(&id).is_empty(),
        "nothing may come back onto the stage"
    );
}

/// R2 — the shape the verdict flagged. A second destination's first sight
/// sealed the session's growth onto the shared stage, so the first
/// destination's stored shard fact lags the body the reclaim then proved and
/// retained; its archive cannot be consulted. The retained summary covers the
/// debt shard for shard, so the debt settles locally — no forced reread, no
/// whole re-export — and the durable map (which holds everything the entry's
/// cache names, plus the second destination's records) is the baseline: the
/// cursor moved and not one record changed, so the pass writes nothing at all.
///
/// Before the coverage rule this was the whole re-export: `lines=1`,
/// `unverified_cursors=1`, a fresh whole-session shard staged onto a stage the
/// reclaim had just emptied — the 11-01 shape in miniature.
#[test]
fn r2_a_covered_but_lagging_reclaimed_debt_settles_and_writes_nothing() {
    let fx = Fixture::new();
    let id = fx.session_id();
    fx.collect();
    let first = fx.debt_shards_of(DESTINATION, &id).expect("setup debt");
    assert_eq!(
        first.get("shard_count").and_then(Value::as_u64),
        Some(1),
        "setup: the first destination's debt names the one whole shard it sealed"
    );
    assert_eq!(fx.stage_shard_bodies(&id).len(), 1);

    // The session grows, and the second destination reads it first: its pass
    // has no entry of its own, so it folds the staged records and delivers
    // only the records that are missing — the growth lands on the shared
    // stage, and the first destination's debt now lags the body.
    add_turn(&fx.db, SESSION, "m2", 3000);
    let second = fx.collect_for(SECOND_DESTINATION);
    assert!(
        outcome_of(&second, &id).shard.is_some(),
        "setup: the second destination sealed the missing records"
    );
    let lagging = fx
        .debt_shards_of(DESTINATION, &id)
        .expect("the first destination's debt");
    assert_eq!(
        lagging.get("shard_count").and_then(Value::as_u64),
        Some(1),
        "setup: the first destination's stored fact still lags the two-shard body"
    );
    let second_debt = fx
        .debt_shards_of(SECOND_DESTINATION, &id)
        .expect("the second destination's debt");
    assert_eq!(
        second_debt.get("shard_count").and_then(Value::as_u64),
        Some(2),
        "setup: the second destination's fact names the whole two-shard stage"
    );
    assert_eq!(fx.stage_shard_bodies(&id).len(), 2);

    fx.push();
    fx.reclaim();
    assert!(fx.stage_shard_bodies(&id).is_empty(), "setup: reclaimed");
    fx.touch_other_session();

    // The pass under test: the first destination, whose archive cannot be
    // consulted. Its debt is covered by the retained summary, so it settles
    // without the archive — and the reclaim-proven map answers the pass, so
    // the unchanged content writes nothing.
    let report = fx.collect_for(DESTINATION);

    assert_eq!(
        report.unverified_cursors, 0,
        "a debt the retained summary covers is not unverifiable"
    );
    let outcome = outcome_of(&report, &id);
    assert_eq!(
        outcome.lines_written, 0,
        "no whole re-export of a session the archives provably hold"
    );
    assert!(outcome.shard.is_none());
    assert!(!outcome.reset);
    assert_eq!(report.reset_records, 0);
    assert!(
        fx.stage_shard_bodies(&id).is_empty(),
        "nothing may come back onto the emptied stage"
    );

    // The mechanism: the row the reclaim retained names, shard for shard,
    // every shard the lagging debt claims.
    let row = fx.manifest_row(&id).expect("the reclaim retained a row");
    let row_identities = row
        .get("shard_identities")
        .and_then(Value::as_array)
        .expect("the retained row carries the per-shard identities the proof covered");
    assert_eq!(row_identities.len(), 2);
    let lagging_bytes = lagging
        .get("shard_identities")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    assert_eq!(lagging_bytes.len(), 1);
    assert!(
        row_identities.contains(&lagging_bytes[0]),
        "the row the reclaim proved names every shard the lagging debt claims"
    );

    // The entry adopted the reclaim-proven map, so the next pass is quiet.
    let again = fx.collect_for(DESTINATION);
    let outcome = outcome_of(&again, &id);
    assert_eq!(outcome.lines_written, 0);
    assert!(outcome.shard.is_none());
    assert!(!outcome.reset);
    assert_eq!(again.unverified_cursors, 0);
}

/// R3 — unknown is never "nothing new". Grown after reclaim with both durable
/// map copies gone, the session can only be observed whole, once: the debt
/// itself settles against the retained row (so `unverified` stays zero and
/// the reset carries the map cause, not the unverifiable word), the one
/// observation re-derives the map, and the next pass is silent.
#[test]
fn r3_without_a_durable_map_a_grown_reclaimed_session_degrades_to_one_named_observation() {
    let fx = Fixture::new();
    let id = fx.session_id();
    fx.collect();
    fx.push();
    fx.reclaim();
    fx.forget_maps(&id);
    add_turn(&fx.db, SESSION, "m2", 3000);

    let report = fx.collect();

    assert_eq!(
        report.unverified_cursors, 0,
        "the debt settles against the retained row"
    );
    let outcome = outcome_of(&report, &id);
    assert_eq!(outcome.lines_written, 1, "one complete observation");
    assert!(outcome.shard.is_some());
    assert!(outcome.reset);
    assert_eq!(report.reset_records, 1);
    assert_eq!(
        report.reset_causes.get(&(
            "opencode".to_string(),
            "opencode_map_unreconciled".to_string()
        )),
        Some(&1),
        "the reset names the map the pass could not reconcile, causes: {:?}",
        report.reset_causes
    );
    let bodies = fx.stage_shard_bodies(&id);
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0], fresh_export_framed(&fx));
    assert!(fx.meta_map(&id).exists(), "the degrade re-derived the map");

    let again = fx.collect();
    let outcome = outcome_of(&again, &id);
    assert_eq!(outcome.lines_written, 0);
    assert!(outcome.shard.is_none());
    assert!(!outcome.reset);
    assert_eq!(fx.stage_shard_bodies(&id).len(), 1);
}
