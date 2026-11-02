//! W1062 — the reclaim's apply window: a shard sealed after the proof must
//! survive `--apply`'s delete.
//!
//! W1057's verdict (M1, its section 3's residual) on the pre-existing
//! reclaim-vs-collect window: `reclaim_stage` proves every candidate, writes
//! the retained manifest, then deletes session bodies one at a time — and
//! the delete removed every sealed shard present *at delete time*, not only
//! the shards the proof's digest triple was computed over. A collect pass
//! in flight meanwhile (the hourly run-once runs for most of the hour; the
//! 11-01 reclaim finished at 04:06:55 inside a pass) seals a new, unpushed
//! increment shard for a session the reclaim has already proved. That shard
//! was never pushed and never proven, and the reclaim deleted it anyway —
//! recoverable only by re-reading the source, and lost if the source has
//! since vanished.
//!
//! Production shape reproduced here, all through the real
//! `reclaim_stage(apply)`:
//!
//! * the store holds an earlier-reclaimed session, so a manifest exists;
//! * two increment-era sessions, pushed and unchanged since their pass —
//!   the state the 11-01 reclaim found;
//! * the concurrent pass seals one more shard for one of them *after* the
//!   reclaim has written its manifest (the proof record) and before that
//!   session's delete, through the real `collect` machinery, the way the
//!   pass in flight did.
//!
//! The fixtures are synthetic SQLite stores in `tempfile` directories;
//! assertions are counts and sequence numbers only.

use crate::collect::{self, CollectReport, DestinationView};
use crate::config::Config;
use crate::destdiag::DestinationHealth;
use crate::manifest::{self, ManifestFileState};
use crate::scanner::{self, HarnessRegistry, ScanReport};
use crate::stagereclaim::{self, NamedStore, ReclaimReport};
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

/// One real collect pass over the synthetic store, from explicit paths, so
/// the race's hook can run it inside the reclaim without borrowing anything.
fn collect_once(db: &Path, stage: &Path, state: &Path) -> CollectReport {
    let mut roots = BTreeMap::new();
    roots.insert("opencode".to_string(), db.to_string_lossy().into_owned());
    let config = Config {
        harness_roots: roots,
        ..Config::default()
    };
    let scan: ScanReport = scanner::scan_with_registry(&config, &sqlite_registry()).unwrap();
    let dest = DestinationView::unreachable(DESTINATION);
    collect::collect_scan_report(&scan, stage, MACHINE, state, 20, &dest).unwrap()
}

struct Fixture {
    stage: PathBuf,
    state: PathBuf,
    db: PathBuf,
    cfg: StoreConfig,
    mk: MasterKey,
    /// Held for the test's lifetime so the paths above stay on disk; the
    /// race's hook gets copies of the paths and needs the directories to
    /// outlive the reclaim it interposes on.
    _dir: tempfile::TempDir,
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
            _dir: dir,
        }
    }

    /// One real collect pass over the store, on this fixture's paths.
    fn collect(&self) -> CollectReport {
        collect_once(&self.db, &self.stage, &self.state)
    }

    fn push(&self) {
        BackupStore::new(self.cfg.clone(), MACHINE.to_string())
            .push(&self.stage, &self.mk)
            .unwrap();
    }
}

fn reclaim_with(stage: &Path, cfg: &StoreConfig) -> ReclaimReport {
    let dests = vec![NamedStore {
        name: "repo-a".to_string(),
        cfg: cfg.clone(),
        health: DestinationHealth::Healthy,
    }];
    let report = stagereclaim::reclaim_stage(stage, &dests, true, None).unwrap();
    assert!(!report.blocked(), "reclaim blocked: {:?}", report.blocked);
    report
}

/// The stage's session directory whose name ends with `suffix` (the harness
/// owns the id's prefix; the fixture owns the tail).
fn session_dir_by_suffix(stage: &Path, suffix: &str) -> PathBuf {
    let machine_dir = stage.join("sessions").join(MACHINE);
    for entry in std::fs::read_dir(&machine_dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir()
            && entry.file_name().to_string_lossy().ends_with(suffix)
        {
            return entry.path();
        }
    }
    panic!("no session directory ends with {suffix}");
}

/// Sealed-shard sequence numbers in one session directory, lowest first.
fn seqs_in(dir: &Path) -> Vec<u64> {
    let mut seqs: Vec<u64> = store::sealed_shard_entries(dir)
        .unwrap()
        .into_iter()
        .map(|(seq, _)| seq)
        .collect();
    seqs.sort_unstable();
    seqs
}

/// The state production was in just before the 11-01 reclaim ran: an
/// earlier reclaim left a manifest that names nothing below, two
/// increment-era sessions are pushed and unchanged since their pass, and
/// the claim on each is a settled cursor over its two sealed shards.
fn pre_reclaim_production_shape() -> Fixture {
    let fx = Fixture::new();
    add_session(&fx.db, "ses_old");
    fx.collect();
    fx.push();
    fx.reclaim();
    assert!(
        seqs_in(&session_dir_by_suffix(&fx.stage, "ses_old")).is_empty(),
        "the earlier reclaim emptied the stage"
    );
    assert!(
        manifest::manifest_path(&fx.stage, MACHINE).is_file(),
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
    let quiet = fx.collect();
    assert_eq!((quiet.reset_records, quiet.shards_written), (0, 0));
    fx
}

impl Fixture {
    fn reclaim(&self) -> ReclaimReport {
        reclaim_with(&self.stage, &self.cfg)
    }

    /// The manifest row for the session whose id ends with `suffix`.
    fn manifest_row(&self, suffix: &str) -> manifest::SessionManifest {
        let rows = match manifest::read_manifest(&self.stage, MACHINE).unwrap() {
            ManifestFileState::Loaded(rows) => rows,
            other => panic!("expected Loaded, got {other:?}"),
        };
        rows.into_iter()
            .find(|row| row.session_id.ends_with(suffix))
            .unwrap_or_else(|| panic!("manifest has no row ending with {suffix}"))
    }
}

/// The 11-01 shape, one session interleaved: the reclaim has surveyed,
/// proven, and written this machine's manifest; a collect pass still in
/// flight then seals one more, unpushed shard for a session the reclaim
/// already proved; the reclaim's delete reaches that session afterwards.
///
/// RED without the narrowed delete: every sealed shard present at delete
/// time was removed, including the one the pass sealed after the proof.
#[test]
fn the_apply_delete_keeps_a_shard_sealed_by_a_concurrent_pass_after_the_proof() {
    let fx = pre_reclaim_production_shape();

    let fired = Rc::new(Cell::new(0_usize));
    let nested_shards = Rc::new(Cell::new(0_usize));
    let hook_fired = Rc::clone(&fired);
    let hook_shards = Rc::clone(&nested_shards);
    let (db, stage, state) = (fx.db.clone(), fx.stage.clone(), fx.state.clone());
    stagereclaim::tests_support::set_before_session_delete(Some(Box::new(move |_, session_id| {
        if !session_id.ends_with("ses_a") || hook_fired.get() > 0 {
            return;
        }
        hook_fired.set(1);
        // The pass in flight: the source grew, the pass reads it and seals
        // the increment — unpushed, after the reclaim's manifest was
        // written and its proof was made, exactly at the delete of the
        // session it belongs to.
        add_turn(&db, "ses_a", "m3", 4000);
        let report = collect_once(&db, &stage, &state);
        hook_shards.set(report.shards_written);
    })));
    let report = reclaim_with(&fx.stage, &fx.cfg);
    stagereclaim::tests_support::set_before_session_delete(None);

    assert_eq!(fired.get(), 1, "the hook fired for one session's delete");
    assert_eq!(
        nested_shards.get(),
        1,
        "the concurrent pass sealed exactly one new shard"
    );

    // The reclaim's proof record predates the growth: the manifest row is
    // the summary of the two proven shards.
    assert_eq!(
        report
            .candidates
            .iter()
            .find(|c| c.session_id.ends_with("ses_a"))
            .unwrap()
            .shard_count,
        2,
        "the survey saw the two shards it proved"
    );
    assert_eq!(
        seqs_in(&session_dir_by_suffix(&fx.stage, "ses_a")),
        vec![3],
        "the shard the concurrent pass sealed after the proof must survive"
    );
    assert_eq!(
        report
            .reclaimed
            .iter()
            .find(|r| r.session_id.ends_with("ses_a"))
            .unwrap()
            .shard_count,
        2,
        "the delete removed only the two proven shards"
    );
    assert_eq!(
        seqs_in(&session_dir_by_suffix(&fx.stage, "ses_b")),
        Vec::<u64>::new(),
        "the session no pass wrote to was reclaimed whole"
    );
    assert_eq!(fx.manifest_row("ses_a").shard_count, 2);
    assert!(
        session_dir_by_suffix(&fx.stage, "ses_a")
            .join(crate::shard_facts::FILE_NAME)
            .is_file(),
        "the digest record stays while the survived body is on stage"
    );

    // The survivor is a normal body from here: the next push takes it to the
    // archive, and the next reclaim proves it against that newest snapshot
    // and reclaims what is by then proven.
    fx.push();
    let second = fx.reclaim();
    assert_eq!(
        second
            .reclaimed
            .iter()
            .find(|r| r.session_id.ends_with("ses_a"))
            .unwrap()
            .shard_count,
        1,
        "the next run reclaims the survivor once it is proven"
    );
    assert!(
        seqs_in(&session_dir_by_suffix(&fx.stage, "ses_a")).is_empty(),
        "the survivor is gone after the run that proved it"
    );
    assert_eq!(fx.manifest_row("ses_a").shard_count, 1);
}
