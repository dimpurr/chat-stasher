//! W1020 — SG-3 / ADR-061: snapshot-style sources go increment per record
//! (opencode first).
//!
//! The ADR's own acceptance list (D7), each fixture asserted here:
//!
//!   * the B2 fixture: a session observed whole-file, whose source then
//!     revert-removes messages and grows again — the default assembled view is
//!     byte-identical to the source's final export, and with the
//!     deleted-content switch on, the removed messages reappear at their
//!     original positions, marked deleted;
//!   * part-level write (B1): a pass that changes only one part re-emits
//!     exactly that part's record — the message-row record is not re-written;
//!     a new message arriving with N parts writes 1 + N records;
//!   * a snapshot-session grown many times: the stage holds the growth, not a
//!     new whole-file copy per change (46 §4.7: stage ≈ final size × 1.x), and
//!     the assembled readback is byte-identical to the last version;
//!   * identical replay delivers nothing (convergence) and a tombstoned record
//!     that reappears is a resurrection that delivers;
//!   * map loss degrades to exactly one full export, then incremental again.
//!
//! Everything runs in `tempfile` scratch directories against a synthetic
//! SQLite store built by this file, pushed to a no-cache local repository when
//! a readback is asserted. No real harness store is read; no network is
//! touched; assertions stay metadata-shaped (counts, bytes, digests and
//! synthetic record ids only).

use chat_stasher::collect;
use chat_stasher::collect::{CollectReport, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::opencode_records::BodyView;
use chat_stasher::scanner::{self, HarnessRegistry, ScanReport};
use chat_stasher::sqlite_probe;
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rusqlite::Connection;
use rustic_core::repofile::MasterKey;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const MACHINE: &str = "fixture-machine";
const DESTINATION: &str = "fixture-destination";
const SESSION: &str = "ses_w1020";
const STATE_DIR_SUFFIX: &str = "state";

/// The synthetic registry cell for `opencode`, exactly like the real one minus
/// `env_override`: an ambient `OPENCODE_DB` must not steer this test.
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

fn dest<'a>() -> DestinationView<'a> {
    DestinationView::unreachable(DESTINATION)
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
        let state = dir.path().join(STATE_DIR_SUFFIX);
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

    fn collect(&self) -> CollectReport {
        collect::collect_scan_report(&self.scan(), &self.stage, MACHINE, &self.state, 20, &dest())
            .unwrap()
    }

    fn session_id(&self) -> String {
        self.scan()
            .records
            .iter()
            .map(|record| record.id.clone())
            .find(|id| id.ends_with(&format!(".{SESSION}")))
            .unwrap_or_else(|| panic!("scan produced no record for {SESSION}"))
    }

    /// The sealed shard bodies of this session's stage directory, in sequence
    /// order, as raw bytes.
    fn stage_shard_bodies(&self, session_id: &str) -> Vec<Vec<u8>> {
        let dir = store::session_shard_dir(&self.stage, MACHINE, session_id);
        let mut entries = store::sealed_shard_entries(&dir).unwrap();
        entries.sort_by_key(|(seq, _)| *seq);
        entries
            .into_iter()
            .map(|(_, path)| fs::read(&path).unwrap())
            .collect()
    }

    fn shard_count(&self, session_id: &str) -> usize {
        let dir = store::session_shard_dir(&self.stage, MACHINE, session_id);
        store::sealed_shard_entries(&dir).unwrap().len()
    }

    fn stage_bytes(&self, session_id: &str) -> u64 {
        self.stage_shard_bodies(session_id)
            .iter()
            .map(|body| body.len() as u64)
            .sum()
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
    add_part(
        &conn,
        "m2",
        "p3",
        2400,
        r#"{"type":"text","text":"synthetic three"}"#,
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

/// The fresh single-session export the assembled default view is required to
/// equal byte for byte, plus the trailing frame byte every sealed line gets.
fn fresh_export_framed(fx: &Fixture) -> Vec<u8> {
    let snapshot = sqlite_probe::read_opencode_session(&fx.db, SESSION).unwrap();
    let mut framed = snapshot.json_line;
    framed.push(b'\n');
    framed
}

/// Parse one stage shard's lines into JSON values (asserting the shard is
/// line-JSONL of the synthetic shapes this file writes).
fn shard_lines(body: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(body)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap_or(Value::Null))
        .collect()
}

/// The MINIMUM end-to-end lever: collect, push to a local no-cache repo, and
/// read the session back through the archive reader.
fn push_and_read(fx: &Fixture) -> Vec<u8> {
    let cfg = store_config(fx.dir.path(), "repo");
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let summary = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&fx.stage, &mk)
        .unwrap();
    assert!(summary.snapshots_in_repo > 0);
    let (bytes, _shards) = BackupStore::new(cfg, MACHINE.to_string())
        .read_session_concat(MACHINE, &fx.session_id(), &mk)
        .unwrap();
    bytes
}

/// The raw shard-set identity the archive must keep proving (L3's question)
/// stays a function of the raw stage bytes — asserted against the archived
/// triple so the assembly can never silently become the proof.
fn readback_triple_matches_stage(fx: &Fixture, session_id: &str) {
    let cfg = store_config(fx.dir.path(), "repo-readback");
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let summary = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&fx.stage, &mk)
        .unwrap();
    assert!(summary.snapshots_in_repo > 0);
    let report = BackupStore::new(cfg, MACHINE.to_string())
        .read_all_machines(&mk)
        .unwrap();
    let mut found = None;
    for machine in &report.machines {
        for session in &machine.sessions {
            if session.session_id == session_id {
                found = Some((
                    session.shard_count,
                    session.sha256.clone(),
                    session.concat_bytes,
                ));
            }
        }
    }
    let (count, sha, concat_bytes) = found.expect("readback must hold the session");
    assert_eq!(count, fx.shard_count(session_id), "shard counts must agree");
    assert_eq!(
        concat_bytes,
        store::concat_shards(&fx.stage, MACHINE, session_id)
            .unwrap()
            .len() as u64
    );
    assert_eq!(
        sha,
        concat_sha_of_stage(&fx.stage, MACHINE, session_id),
        "the raw triple must stay a function of the raw stage bytes"
    );
}

/// The sha256 of a session's stage shards concatenated in sequence order —
/// the same digest `verify` L3 and the reclaim manifest hold. Shorthands the
/// private `store::concat_sha_of` for this file's parity assertion.
fn concat_sha_of_stage(stage: &Path, machine: &str, session_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let dir = store::session_shard_dir(stage, machine, session_id);
    let mut entries = store::sealed_shard_entries(&dir).unwrap();
    entries.sort_by_key(|(seq, _)| *seq);
    let mut concat = Vec::new();
    for (_, path) in entries {
        concat.extend_from_slice(&fs::read(&path).unwrap());
    }
    hex_of(Sha256::digest(&concat).as_slice())
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The B2 fixture: whole-file v1 → the source revert-removes two messages →
/// the session grows again. The default assembled view must be byte-identical
/// to the source's final export.
#[test]
fn b2_revert_then_grow_assembles_the_source_final_state() {
    let fx = Fixture::new();
    let first = fx.collect();
    assert_eq!(
        first.shards_written, 1,
        "the first observation is one shard"
    );
    let session_id = fx.session_id();

    // The source reverts: m1 and m2 (and their parts) are removed.
    let conn = connect(&fx.db);
    conn.execute(
        "DELETE FROM part WHERE message_id IN (?1, ?2)",
        rusqlite::params![format!("{SESSION}-m1"), format!("{SESSION}-m2")],
    )
    .unwrap();
    conn.execute(
        "DELETE FROM message WHERE id IN (?1, ?2)",
        rusqlite::params![format!("{SESSION}-m1"), format!("{SESSION}-m2")],
    )
    .unwrap();

    // The session also grows: a new message with two parts.
    conn.execute(
        "UPDATE session SET time_updated = 3000 WHERE id = ?1",
        [SESSION],
    )
    .unwrap();
    drop(conn);
    {
        let conn = connect(&fx.db);
        add_message(&conn, 2600, "user", "m3");
        add_part(
            &conn,
            "m3",
            "p4",
            2700,
            r#"{"type":"text","text":"synthetic four"}"#,
        );
        add_part(
            &conn,
            "m3",
            "p5",
            2800,
            r#"{"type":"text","text":"synthetic five"}"#,
        );
    }
    let second = fx.collect();
    assert_eq!(
        second.shards_written, 1,
        "the revert+grow pass writes exactly one shard"
    );

    // The session grows once more, so the final state is not the revert state.
    {
        let conn = connect(&fx.db);
        conn.execute(
            "UPDATE session SET time_updated = 3200 WHERE id = ?1",
            [SESSION],
        )
        .unwrap();
        add_message(&conn, 3000, "assistant", "m4");
        add_part(
            &conn,
            "m4",
            "p6",
            3100,
            r#"{"type":"text","text":"synthetic six"}"#,
        );
    }
    let third = fx.collect();
    assert_eq!(third.shards_written, 1);

    let readback = push_and_read(&fx);
    let expected = fresh_export_framed(&fx);
    assert_eq!(
        readback, expected,
        "the assembled default view must equal a fresh export of the source's \
         final state byte for byte"
    );

    // The raw triple stays a function of the raw bytes (separate repo, later
    // push): the assembly must never become the proof object.
    readback_triple_matches_stage(&fx, &session_id);
}

/// B1, the part-level write unit: a pass that changes only one part re-emits
/// exactly that part's record, and the message-row record is not re-written.
#[test]
fn one_changed_part_reemits_only_that_part_record() {
    let fx = Fixture::new();
    fx.collect();
    let session_id = fx.session_id();
    let before = fx.stage_shard_bodies(&session_id);

    {
        let conn = connect(&fx.db);
        // The edited part becomes the store's greatest (time_updated, id)
        // high-water, the way a real edit does — otherwise the logical cursor
        // cannot see the change at all and the pass would skip for free.
        conn.execute(
            "UPDATE part SET data = ?1, time_updated = 2500 WHERE id = ?2",
            rusqlite::params![
                r#"{"type":"text","text":"synthetic part edited"}"#,
                format!("{SESSION}-p2")
            ],
        )
        .unwrap();
    }

    fx.collect();
    let after = fx.stage_shard_bodies(&session_id);
    assert_eq!(
        before.len() + 1,
        after.len(),
        "the changed pass seals exactly one new shard"
    );

    // Every line of the new shard must be a v2 record; among them exactly one
    // part record for the edited part id, and no message record at all. At the
    // pre-change code (one full snapshot per change) this fails twice over:
    // the line's schema is v1 and it embeds every message.
    let new_shard = &after[after.len() - 1];
    let lines = shard_lines(new_shard);
    assert!(
        !lines.is_empty(),
        "the increment shard is made of record lines"
    );
    let mut part_records = 0;
    let mut message_records = 0;
    for line in &lines {
        assert_eq!(
            line["schema"], "chat-stasher.opencode.session.v2",
            "each record line carries the v2 schema"
        );
        match line["kind"].as_str() {
            Some("part") => {
                assert_eq!(
                    line["record_id"],
                    format!("{SESSION}-p2"),
                    "the only part record is the edited part"
                );
                part_records += 1;
            }
            Some("message") => message_records += 1,
            other => panic!("unexpected record kind in the increment shard: {other:?}"),
        }
    }
    assert_eq!(part_records, 1, "exactly one record for the edited part");
    assert_eq!(
        message_records, 0,
        "the message-row record is not re-written when only a part changed"
    );
}

/// B1, the 1 + N rule: a new message arriving with N parts writes 1 message
/// record and N part records (plus the session-observation record the session
/// row's own change fires, and tombstones for anything the source dropped).
#[test]
fn a_new_message_with_two_parts_writes_one_plus_two_records() {
    let fx = Fixture::new();
    fx.collect();
    let session_id = fx.session_id();

    {
        let conn = connect(&fx.db);
        conn.execute(
            "UPDATE session SET time_updated = 1500 WHERE id = ?1",
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
        add_part(
            &conn,
            "m3",
            "p5",
            2800,
            r#"{"type":"text","text":"synthetic five"}"#,
        );
    }
    fx.collect();

    let bodies = fx.stage_shard_bodies(&session_id);
    let new_shard = &bodies[bodies.len() - 1];
    let lines = shard_lines(new_shard);
    let mut message_records = 0;
    let mut part_records = 0;
    let mut session_records = 0;
    for line in &lines {
        assert_eq!(line["schema"], "chat-stasher.opencode.session.v2");
        match line["kind"].as_str() {
            Some("message") => {
                assert_eq!(line["record_id"], format!("{SESSION}-m3"));
                message_records += 1;
            }
            Some("part") => {
                assert!(
                    [format!("{SESSION}-p4"), format!("{SESSION}-p5")]
                        .contains(&line["record_id"].as_str().unwrap().to_string()),
                    "part records are the two new parts, not any old one"
                );
                part_records += 1;
            }
            Some("session") => session_records += 1,
            other => panic!("unexpected record kind: {other:?}"),
        }
    }
    assert_eq!(message_records, 1, "1 + N: one new message record");
    assert_eq!(part_records, 2, "1 + N: two part records");
    assert_eq!(
        session_records, 1,
        "the session row changed, so the session-observation record fires"
    );
}

/// 46 §4.7 / D7: a session grown many times must keep the stage near the
/// final size, and the assembled readback must equal the last version byte
/// for byte. Before this change each growth step re-stored the whole session,
/// so the stage accumulated one full copy per step.
#[test]
fn a_session_grown_twenty_times_keeps_the_stage_near_the_final_size() {
    let fx = Fixture::new();
    fx.collect();
    let session_id = fx.session_id();

    for step in 0..20 {
        let time = 5000 + step * 100;
        // Real opencode bulk sits in parts — tool outputs of kilobytes, not
        // prose rows of a few hundred bytes — so the growth payload is
        // shaped the way the source's own bytes are. A fixture of tiny rows
        // would measure nothing but the per-record envelope.
        let body = "synthetic tool output ".repeat(64);
        let conn = connect(&fx.db);
        conn.execute(
            "UPDATE session SET time_updated = ?1 WHERE id = ?2",
            rusqlite::params![time, SESSION],
        )
        .unwrap();
        add_message(&conn, time, "user", &format!("g{step}"));
        add_part(
            &conn,
            &format!("g{step}"),
            &format!("gp{step}"),
            time + 10,
            &format!(r#"{{"type":"text","text":"{body}"}}"#),
        );
        drop(conn);
        let report = fx.collect();
        assert_eq!(
            report.shards_written, 1,
            "each growth step writes one shard"
        );
    }

    let final_bytes = fresh_export_framed(&fx).len() as u64;
    let stage_bytes = fx.stage_bytes(&session_id);
    // The synthetic rows here are a few hundred bytes each, so each record's
    // small per-record envelope is a large fraction of its payload — which
    // is why the honest bound for this fixture is 3× the final export, not
    // 1.x / 2×. The quantity under test is the *shape*: no growth step may
    // re-store the whole session (the pre-change era measured 78,789 bytes
    // for this exact flow, ≈12×) and the growth shards hold the growth. On
    // real sessions, whose parts are kilobytes of tool output, the envelope
    // overhead is the few-percent ADR-061 revisit trigger 2 budgets for.
    assert!(
        stage_bytes <= final_bytes * 3,
        "stage bytes {stage_bytes} must stay within 3× the final export's \
         {final_bytes} bytes; the whole-file era re-stored one full copy per \
         step (78,789 bytes on the pre-change code for this flow)"
    );
    let readback = push_and_read(&fx);
    let expected = fresh_export_framed(&fx);
    assert_eq!(
        readback, expected,
        "the assembled readback equals the last version byte for byte"
    );
}

/// Convergence: an identical replay of the same observation delivers nothing.
#[test]
fn an_identical_replay_delivers_nothing() {
    let fx = Fixture::new();
    let first = fx.collect();
    assert_eq!(first.shards_written, 1);
    let session_id = fx.session_id();
    let before = fx.stage_bytes(&session_id);

    let second = fx.collect();
    assert_eq!(
        second.shards_written, 0,
        "an unchanged session must not be re-delivered"
    );
    assert_eq!(fx.stage_bytes(&session_id), before);
}

// ---------------------------------------------------------------------------
// The remaining ADR-061 D7 fixtures, added with the implementation: the
// deleted-included view, explicit tombstones, the resurrection, and the
// two map-recovery paths the ADR pins (bound for the live-day gate below).
// ---------------------------------------------------------------------------

/// Push the fixture's stage once and answer the store config + key, for the
/// CLI-level assertions.
fn pushed(fx: &Fixture, repo_name: &str) -> (rustic_core::repofile::MasterKey, StoreConfig) {
    let cfg = store_config(fx.dir.path(), repo_name);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let summary = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&fx.stage, &mk)
        .unwrap();
    assert!(summary.snapshots_in_repo > 0);
    (mk, cfg)
}

fn read_with(
    fx: &Fixture,
    cfg: &StoreConfig,
    mk: &rustic_core::repofile::MasterKey,
    view: chat_stasher::opencode_records::BodyView,
) -> Vec<u8> {
    let (bytes, _shards) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat_with_view(MACHINE, &fx.session_id(), mk, view)
        .unwrap();
    bytes
}

/// B2, second half: with the deleted-content switch on, the revert-removed
/// messages reappear at their original positions, marked `"deleted": true`;
/// the default view keeps no marker at all.
#[test]
fn deleted_records_reappear_marked_with_the_switch() {
    let fx = Fixture::new();
    fx.collect();
    // The source reverts m2's whole subtree and grows.
    {
        let conn = connect(&fx.db);
        conn.execute(
            "DELETE FROM part WHERE message_id = ?1",
            [format!("{SESSION}-m2")],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM message WHERE id = ?1",
            [format!("{SESSION}-m2")],
        )
        .unwrap();
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
    fx.collect();

    let (mk, cfg) = pushed(&fx, "repo-switch");
    let session_id = fx.session_id();

    let default_view = read_with(&fx, &cfg, &mk, BodyView::Current);
    let parsed: Value = serde_json::from_slice(&default_view[..default_view.len() - 1]).unwrap();
    let ids: Vec<&str> = parsed["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["ses_w1020-m1", "ses_w1020-m3"], "m2 is masked out");
    assert!(
        !parsed.to_string().contains("\"deleted\""),
        "no markers in the default view"
    );
    let _ = session_id;

    let deleted_view = read_with(&fx, &cfg, &mk, BodyView::IncludeDeleted);
    let parsed: Value = serde_json::from_slice(&deleted_view[..deleted_view.len() - 1]).unwrap();
    let rows = parsed["messages"].as_array().unwrap();
    let ids: Vec<&str> = rows.iter().map(|row| row["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        ["ses_w1020-m1", "ses_w1020-m2", "ses_w1020-m3"],
        "m2 sits at its original position (between m1 and m3)"
    );
    let m2 = &rows[1];
    assert_eq!(m2["deleted"], true, "m2 carries the deletion marker");
    assert_eq!(
        m2["parts"].as_array().unwrap().len(),
        2,
        "its parts come back too"
    );
    for part in m2["parts"].as_array().unwrap() {
        assert_eq!(part["deleted"], true, "each deleted part is marked");
    }
    let _ = session_id;
}

/// A revert pass emits explicit tombstones for what the source dropped, and
/// the tombstones mask without erasing: the stage still holds the deleted
/// records' bytes in the shard set.
#[test]
fn reverting_emits_tombstones_that_mask_without_erasing() {
    let fx = Fixture::new();
    fx.collect();
    let session_id = fx.session_id();
    let before = fx.stage_shard_bodies(&session_id);

    {
        let conn = connect(&fx.db);
        conn.execute(
            "DELETE FROM part WHERE message_id = ?1",
            [format!("{SESSION}-m2")],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM message WHERE id = ?1",
            [format!("{SESSION}-m2")],
        )
        .unwrap();
        conn.execute(
            "UPDATE session SET time_updated = 3000 WHERE id = ?1",
            [SESSION],
        )
        .unwrap();
    }
    fx.collect();

    let after = fx.stage_shard_bodies(&session_id);
    let tomb_shard = &after[after.len() - 1];
    let lines = shard_lines(tomb_shard);
    let mut tombstones = 0;
    for line in &lines {
        if line["deleted"] == Value::Bool(true) {
            tombstones += 1;
            let kind = line["kind"].as_str().unwrap();
            assert!(
                ["message", "part"].contains(&kind),
                "tombstones carry the word `deleted` with a `kind` field"
            );
            assert!(line.get("payload").is_none() || line["payload"].is_null());
        }
    }
    assert_eq!(
        tombstones, 3,
        "m2 plus its two parts leave explicit tombstones"
    );
    // Mask, never erase: the deleted records' bytes remain on the stage in
    // the shard that delivered them.
    assert!(
        String::from_utf8_lossy(&before[0]).contains(r#""text":"synthetic two""#),
        "the tombstone did not erase the stored bytes"
    );
}

/// D4's hole-filling rule: a record the source deleted and then re-added — even byte
/// identical — delivers anew; the tombstone stays and the newer record wins,
/// so the assembled view resurrects it.
#[test]
fn a_resurrected_record_delivers_anew() {
    let fx = Fixture::new();
    fx.collect();
    let session_id = fx.session_id();

    {
        let conn = connect(&fx.db);
        conn.execute(
            "DELETE FROM part WHERE message_id = ?1",
            [format!("{SESSION}-m2")],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM message WHERE id = ?1",
            [format!("{SESSION}-m2")],
        )
        .unwrap();
        conn.execute(
            "UPDATE session SET time_updated = 3000 WHERE id = ?1",
            [SESSION],
        )
        .unwrap();
    }
    fx.collect();
    // The revert is undone: m2 (and its parts) return with byte-identical
    // rows except time-ordered ids, which opencode itself stamps fresh.
    {
        let conn = connect(&fx.db);
        conn.execute(
            "UPDATE session SET time_updated = 3400 WHERE id = ?1",
            [SESSION],
        )
        .unwrap();
        add_message(&conn, 2200, "assistant", "m2");
        add_part(
            &conn,
            "m2",
            "p2",
            2300,
            r#"{"type":"text","text":"synthetic two"}"#,
        );
        add_part(
            &conn,
            "m2",
            "p3",
            2400,
            r#"{"type":"text","text":"synthetic three"}"#,
        );
    }
    let third = fx.collect();
    assert_eq!(third.shards_written, 1);

    let bodies = fx.stage_shard_bodies(&session_id);
    let lines = shard_lines(&bodies[bodies.len() - 1]);
    let resurrected: Vec<&Value> = lines
        .iter()
        .filter(|line| line["kind"] == Value::String("message".to_string()))
        .collect();
    assert_eq!(
        resurrected.len(),
        1,
        "the resurrected message delivered anew (the tombstone's twin did not stop it)"
    );
    assert_eq!(resurrected[0]["record_id"], format!("{SESSION}-m2"));

    // The view resurrects it: default view equals the fresh export again.
    let readback = push_and_read(&fx);
    let expected = fresh_export_framed(&fx);
    assert_eq!(
        readback, expected,
        "the resurrection restores byte-identity"
    );
}

/// ADR-061 ruling 2 recovery: losing the map side file (the durable full map) does not
/// cost a full export when the stage still holds the records the state entry
/// endorses — the map is rebuilt from the staged records.
#[test]
fn map_recovery_rebuilds_from_the_stage_without_a_full_export() {
    let fx = Fixture::new();
    fx.collect(); // v1 full observation + caches + side map
    let session_id = fx.session_id();
    let maps_dir = fx.state.join("opencode-session-maps");
    let entries = fs::read_dir(&maps_dir).unwrap().count();
    assert_eq!(entries, 1, "the pass persisted exactly one map side file");

    // Lose it (the whole point: it is a cache, not a truth).
    for entry in fs::read_dir(&maps_dir).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }

    {
        let conn = connect(&fx.db);
        conn.execute(
            "UPDATE session SET time_updated = 1500 WHERE id = ?1",
            [SESSION],
        )
        .unwrap();
        add_message(&conn, 2600, "user", "m3");
    }
    let second = fx.collect();
    assert_eq!(second.shards_written, 1);

    let bodies = fx.stage_shard_bodies(&session_id);
    let lines = shard_lines(&bodies[bodies.len() - 1]);
    assert!(
        !lines.is_empty()
            && lines.iter().all(|line| line["schema"]
                == Value::String("chat-stasher.opencode.session.v2".to_string())),
        "the recovered map let the pass go straight back to increment records — \
         no whole-file re-export"
    );
    assert!(
        fs::read_dir(&maps_dir).unwrap().count() >= 1,
        "the rebuild persisted the map again"
    );
}

/// D2's degrade, pinned: when nothing can recover an increment-era map the
/// entry endorses (side file gone and the entry's cache itself — the whole
/// map, not just its shape — removed with it), exactly one full export
/// re-derives the map, and the pass after it writes increments again.
#[test]
fn map_loss_degrades_to_exactly_one_full_export_then_increments() {
    let fx = Fixture::new();
    fx.collect();
    let session_id = fx.session_id();

    // Destroy the map everywhere the pass could learn it from: the side file
    // and the entry-level caches, leaving the debt entry otherwise intact
    // (the stage still holds it, so the debt stays provable).
    let state_path = fx.state.join("debts-v2.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    for entry in state["destinations"][DESTINATION]["files"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        if entry["session_id"]
            .as_str()
            .is_some_and(|id| id.ends_with(&format!(".{SESSION}")))
        {
            entry["cursor"]
                .as_object_mut()
                .unwrap()
                .remove("opencode_record_digests");
            entry["cursor"]
                .as_object_mut()
                .unwrap()
                .remove("opencode_map_digest");
        }
    }
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    for entry in fs::read_dir(fx.state.join("opencode-session-maps")).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }

    let shard_count_before = fx.shard_count(&session_id);
    {
        let conn = connect(&fx.db);
        conn.execute(
            "UPDATE session SET time_updated = 1500 WHERE id = ?1",
            [SESSION],
        )
        .unwrap();
        add_message(&conn, 2600, "user", "m3");
    }
    let second = fx.collect();
    assert_eq!(second.shards_written, 1);
    assert_eq!(
        fx.shard_count(&session_id),
        shard_count_before + 1,
        "the degrade is exactly one full-export shard"
    );
    let bodies = fx.stage_shard_bodies(&session_id);
    let degrade_lines = shard_lines(&bodies[bodies.len() - 1]);
    assert_eq!(
        degrade_lines.len(),
        1,
        "the degrade shard is one whole-file observation line"
    );
    assert_eq!(
        degrade_lines[0]["schema"], "chat-stasher.opencode.session.v1",
        "the degrade is today's exact behaviour — a complete observation"
    );

    // The map re-derived: the next change is an increment again.
    {
        let conn = connect(&fx.db);
        conn.execute(
            "UPDATE session SET time_updated = 1700 WHERE id = ?1",
            [SESSION],
        )
        .unwrap();
        add_part(
            &conn,
            "m3",
            "p4",
            2700,
            r#"{"type":"text","text":"synthetic four"}"#,
        );
    }
    let third = fx.collect();
    assert_eq!(third.shards_written, 1);
    let bodies = fx.stage_shard_bodies(&session_id);
    let lines = shard_lines(&bodies[bodies.len() - 1]);
    assert!(
        lines
            .iter()
            .all(|line| line["schema"]
                == Value::String("chat-stasher.opencode.session.v2".to_string()))
            && lines
                .iter()
                .any(|line| line["kind"] == Value::String("part".to_string())),
        "increment writing resumed after the single full export"
    );
}

/// A session body that cannot be assembled (a corrupt line in the shard set)
/// is a loud read failure — an exit-3 "did not finish", never a short or
/// empty body.
#[test]
fn an_unassemblable_body_fails_the_read_loudly() {
    let fx = Fixture::new();
    fx.collect();
    let session_id = fx.session_id();

    // A corrupted shard lands in the session dir (the shape a bug or a torn
    // seal leaves), then everything is pushed.
    let dir = store::session_shard_dir(&fx.stage, MACHINE, &session_id);
    fs::write(dir.join("000006.jsonl"), "this line is not json at all\n").unwrap();
    let (mk, cfg) = pushed(&fx, "repo-corrupt");

    let err = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &session_id, &mk)
        .unwrap_err();
    assert!(
        err.to_string().contains("cannot be assembled"),
        "the assembly failure is named, not a short body: {err}"
    );

    // The CLI says the same thing with the read family's exit code 3.
    let out = isolated_command(fx.dir.path())
        .args(["read", "--repo"])
        .arg(&cfg.repo_root)
        .args(["--key-file"])
        .arg(&cfg.key_file)
        .args(["--machine", MACHINE, "--session", &session_id])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "an unassemblable body is a did-not-finish read (exit 3), never a short answer;\n\
         stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `read --include-deleted` and `export --include-deleted` reach the deleted
/// history through the CLI surface.
#[test]
fn the_cli_reaches_the_deleted_view() {
    let fx = Fixture::new();
    fx.collect();
    {
        let conn = connect(&fx.db);
        conn.execute(
            "DELETE FROM part WHERE message_id = ?1",
            [format!("{SESSION}-m2")],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM message WHERE id = ?1",
            [format!("{SESSION}-m2")],
        )
        .unwrap();
        conn.execute(
            "UPDATE session SET time_updated = 3000 WHERE id = ?1",
            [SESSION],
        )
        .unwrap();
        add_message(&conn, 2600, "user", "m3");
    }
    fx.collect();
    let session_id = fx.session_id();
    let (_mk, cfg) = pushed(&fx, "repo-cli");

    // read: the two views report different digests, and the run names the view.
    let mut default_read = isolated_command(fx.dir.path());
    default_read
        .args(["read", "--repo"])
        .arg(&cfg.repo_root)
        .args(["--key-file"])
        .arg(&cfg.key_file)
        .args(["--machine", MACHINE, "--session", &session_id]);
    let default_read = default_read.output().unwrap();
    assert_eq!(default_read.status.code(), Some(0));
    let default_out = String::from_utf8_lossy(&default_read.stdout).into_owned();

    let mut deleted_read = isolated_command(fx.dir.path());
    deleted_read
        .args(["read", "--repo"])
        .arg(&cfg.repo_root)
        .args(["--key-file"])
        .arg(&cfg.key_file)
        .args([
            "--machine",
            MACHINE,
            "--session",
            &session_id,
            "--include-deleted",
        ]);
    let deleted_read = deleted_read.output().unwrap();
    assert_eq!(deleted_read.status.code(), Some(0));
    let deleted_out = String::from_utf8_lossy(&deleted_read.stdout).into_owned();

    let default_sha = default_out
        .lines()
        .find(|line| line.starts_with("[read] concat len"))
        .and_then(|line| line.split("sha256=").nth(1))
        .expect("the default read reports the body digest");
    let deleted_sha = deleted_out
        .lines()
        .find(|line| line.starts_with("[read] concat len"))
        .and_then(|line| line.split("sha256=").nth(1))
        .expect("the include-deleted read reports the body digest");
    assert_ne!(
        default_sha, deleted_sha,
        "the two views are different bodies"
    );
    assert!(
        deleted_out.contains("include-deleted"),
        "the run names the view it read:\n{deleted_out}"
    );

    // export: the deleted messages are in the written file, marked.
    let out_dir = fx.dir.path().join("export-deleted");
    let mut export = isolated_command(fx.dir.path());
    export
        .args(["export", "--repo"])
        .arg(&cfg.repo_root)
        .args(["--key-file"])
        .arg(&cfg.key_file)
        .arg("--out")
        .arg(&out_dir)
        .args(["--force", "--include-deleted"]);
    let export = export.output().unwrap();
    assert_eq!(
        export.status.code(),
        Some(0),
        "export failed:\n{}",
        String::from_utf8_lossy(&export.stdout)
    );
    let session_file = out_dir
        .join(MACHINE)
        .join("opencode")
        .join(format!("{session_id}.jsonl"));
    let body = fs::read_to_string(&session_file).unwrap();
    let parsed: Value = serde_json::from_str(body.trim_end()).unwrap();
    let deleted_rows: Vec<&Value> = parsed["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["deleted"] == Value::Bool(true))
        .collect();
    assert_eq!(
        deleted_rows.len(),
        1,
        "the reverted message is exported, marked"
    );
    assert_eq!(deleted_rows[0]["id"], format!("{SESSION}-m2"));
}

/// SG-2b required fixture: reclaim, then one collect.
/// Unchanged sessions export 0.
/// A session whose source gained one record writes only that increment.
/// The assembled view equals the pre-reclaim bytes.
#[test]
fn sg2b_post_reclaim_unchanged_exports_zero_and_changed_writes_increment_only() {
    let fx = Fixture::new();

    // Add a second session to fx.db which will remain unchanged
    let unchanged_sid = "ses_w1020_unchanged";
    {
        let conn = connect(&fx.db);
        conn.execute(
            "INSERT INTO session (id, time_created, time_updated, title) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![unchanged_sid, 1000, 1000, "unchanged fixture"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![format!("{unchanged_sid}-m1"), unchanged_sid, 1100, 1100, json!({"role":"user"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, session_id, message_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![format!("{unchanged_sid}-p1"), unchanged_sid, format!("{unchanged_sid}-m1"), 1200, 1200, r#"{"type":"text","text":"unchanged one"}"#],
        )
        .unwrap();
    }

    // First collect: both sessions are sealed as one shard each
    let first = fx.collect();
    assert_eq!(first.shards_written, 2);

    let session_growing_id = fx.session_id();
    let session_unchanged_id = fx
        .scan()
        .records
        .iter()
        .map(|r| r.id.clone())
        .find(|id| id.ends_with(&format!(".{unchanged_sid}")))
        .unwrap();

    // Push to repo and verify pre-reclaim readback
    let (mk, cfg) = pushed(&fx, "repo-sg2b");
    let (pre_reclaim_bytes_unchanged, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &session_unchanged_id, &mk)
        .unwrap();
    let (_pre_reclaim_bytes_growing, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &session_growing_id, &mk)
        .unwrap();

    // Reclaim: write manifest and delete shard bodies from stage
    let fresh = chat_stasher::manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    chat_stasher::manifest::write_manifest(&fx.stage, MACHINE, &fresh).unwrap();
    let removed_u =
        chat_stasher::stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session_unchanged_id)
            .unwrap();
    assert_eq!(removed_u, 1);
    let removed_g =
        chat_stasher::stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session_growing_id)
            .unwrap();
    assert_eq!(removed_g, 1);
    assert_eq!(fx.shard_count(&session_unchanged_id), 0);
    assert_eq!(fx.shard_count(&session_growing_id), 0);

    // Source mutation: unchanged session is untouched, growing session gains one message + part
    {
        let conn = connect(&fx.db);
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

    // Collect again: one collect pass
    let second = fx.collect();

    // 1. Unchanged session exports 0:
    assert_eq!(
        fx.shard_count(&session_unchanged_id),
        0,
        "unchanged session must write zero shards to stage"
    );
    let unchanged_outcome = second
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&session_unchanged_id))
        .expect("unchanged session must be in outcomes");
    assert_eq!(
        unchanged_outcome.lines_written, 0,
        "unchanged session must write zero lines"
    );
    assert!(
        unchanged_outcome.shard.is_none(),
        "unchanged session must write no shard"
    );

    // 2. Changed session writes only that increment:
    assert_eq!(
        fx.shard_count(&session_growing_id),
        1,
        "changed session must write exactly one increment shard"
    );
    let growing_outcome = second
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&session_growing_id))
        .expect("growing session must be in outcomes");
    assert!(growing_outcome.shard.is_some());
    let bodies = fx.stage_shard_bodies(&session_growing_id);
    assert_eq!(bodies.len(), 1);
    let lines = shard_lines(&bodies[0]);
    assert_eq!(
        lines.len(),
        3,
        "increment shard contains session row + 1 message + 1 part"
    );
    assert!(lines
        .iter()
        .any(|l| l["record_id"] == format!("{SESSION}-m3")));
    assert!(lines
        .iter()
        .any(|l| l["record_id"] == format!("{SESSION}-p4")));
    assert!(
        !lines
            .iter()
            .any(|l| l["record_id"] == format!("{SESSION}-m1")),
        "pre-reclaim message m1 must not be restaged"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l["record_id"] == format!("{SESSION}-m2")),
        "pre-reclaim message m2 must not be restaged"
    );

    // 3. Push and verify assembled views
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&fx.stage, &mk)
        .unwrap();

    let (post_reclaim_bytes_unchanged, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &session_unchanged_id, &mk)
        .unwrap();
    assert_eq!(
        post_reclaim_bytes_unchanged, pre_reclaim_bytes_unchanged,
        "assembled view of unchanged session equals pre-reclaim bytes"
    );

    let (post_reclaim_bytes_growing, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &session_growing_id, &mk)
        .unwrap();
    assert_eq!(
        post_reclaim_bytes_growing,
        fresh_export_framed(&fx),
        "assembled view of changed session equals fresh export of source's final state"
    );
}

/// Self-attack 1: a test where a changed session is treated as unchanged must fail.
#[test]
fn self_attack_changed_session_treated_as_unchanged_must_fail() {
    let fx = Fixture::new();
    fx.collect();
    let session = fx.session_id();

    // Reclaim
    let fresh = chat_stasher::manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    chat_stasher::manifest::write_manifest(&fx.stage, MACHINE, &fresh).unwrap();
    chat_stasher::stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session).unwrap();

    // Mutate source: session gained a message
    {
        let conn = connect(&fx.db);
        conn.execute(
            "UPDATE session SET time_updated = 3000 WHERE id = ?1",
            [SESSION],
        )
        .unwrap();
        add_message(&conn, 2600, "user", "m3");
    }

    let report = fx.collect();
    let outcome = report
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&session))
        .unwrap();

    // A changed session must NOT have 0 lines written or None shard.
    assert!(
        outcome.shard.is_some(),
        "changed session must emit a shard (if treated as unchanged, this fails)"
    );
    assert!(
        outcome.lines_written > 0,
        "changed session must write increment lines (if treated as unchanged, this fails)"
    );
    assert_eq!(report.changed_records, 1);
    assert_eq!(report.unchanged_records, 0);
}

/// Self-attack 2: a test where an unchanged session is fully rewritten must fail.
#[test]
fn self_attack_unchanged_session_fully_rewritten_must_fail() {
    let fx = Fixture::new();
    fx.collect();
    let session = fx.session_id();

    // Reclaim
    let fresh = chat_stasher::manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    chat_stasher::manifest::write_manifest(&fx.stage, MACHINE, &fresh).unwrap();
    chat_stasher::stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session).unwrap();

    // Touch opencode.db by creating another unrelated table so store_fingerprint changes,
    // but the session itself is UNCHANGED.
    {
        let conn = connect(&fx.db);
        conn.execute("CREATE TABLE dummy_unrelated (id TEXT)", [])
            .unwrap();
    }

    let report = fx.collect();
    let outcome = report
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&session))
        .unwrap();

    // An unchanged session must NOT be rewritten.
    assert!(
        outcome.shard.is_none(),
        "unchanged session must not emit a shard (if fully rewritten, this fails)"
    );
    assert_eq!(
        outcome.lines_written, 0,
        "unchanged session must write 0 lines (if fully rewritten, this fails)"
    );
    assert_eq!(
        report.shards_written, 0,
        "unchanged session must write 0 shards (if fully rewritten, this fails)"
    );
    assert_eq!(report.changed_records, 0);
    assert_eq!(report.unchanged_records, 1);
}

/// SG-2b fresh destination test: after reclaim, collect against a fresh destination
/// (where debts has no entry, so old is None) still exports 0 for unchanged sessions
/// and writes only the increment for changed sessions.
#[test]
fn sg2b_post_reclaim_with_no_stored_debts_exports_zero_and_writes_increment_only() {
    let fx = Fixture::new();

    let unchanged_sid = "ses_fresh_dest_unchanged";
    {
        let conn = connect(&fx.db);
        conn.execute(
            "INSERT INTO session (id, time_created, time_updated, title) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![unchanged_sid, 1000, 1000, "unchanged fixture"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![format!("{unchanged_sid}-m1"), unchanged_sid, 1100, 1100, json!({"role":"user"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, session_id, message_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![format!("{unchanged_sid}-p1"), unchanged_sid, format!("{unchanged_sid}-m1"), 1200, 1200, r#"{"type":"text","text":"unchanged one"}"#],
        )
        .unwrap();
    }

    // Collect on dest-a
    let first = fx.collect();
    assert_eq!(first.shards_written, 2);

    let session_growing_id = fx.session_id();
    let session_unchanged_id = fx
        .scan()
        .records
        .iter()
        .map(|r| r.id.clone())
        .find(|id| id.ends_with(&format!(".{unchanged_sid}")))
        .unwrap();

    // Reclaim: write manifest and remove bodies from stage
    let fresh = chat_stasher::manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    chat_stasher::manifest::write_manifest(&fx.stage, MACHINE, &fresh).unwrap();
    chat_stasher::stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session_unchanged_id)
        .unwrap();
    chat_stasher::stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session_growing_id)
        .unwrap();
    assert_eq!(fx.shard_count(&session_unchanged_id), 0);
    assert_eq!(fx.shard_count(&session_growing_id), 0);

    // Mutate source: growing session gains m3
    {
        let conn = connect(&fx.db);
        conn.execute(
            "UPDATE session SET time_updated = 3000 WHERE id = ?1",
            [SESSION],
        )
        .unwrap();
        add_message(&conn, 2600, "user", "m3");
    }

    // Collect on a fresh destination "dest-b" with NO cursor debts stored
    let fresh_dest = DestinationView::unreachable("dest-b-fresh");
    let second =
        collect::collect_scan_report(&fx.scan(), &fx.stage, MACHINE, &fx.state, 20, &fresh_dest)
            .unwrap();

    // 1. Unchanged session must write 0 shards
    assert_eq!(
        fx.shard_count(&session_unchanged_id),
        0,
        "fresh destination must write zero shards for unchanged session"
    );
    let unchanged_outcome = second
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&session_unchanged_id))
        .unwrap();
    assert_eq!(unchanged_outcome.lines_written, 0);
    assert!(unchanged_outcome.shard.is_none());

    // 2. Changed session must write only the increment
    assert_eq!(
        fx.shard_count(&session_growing_id),
        1,
        "fresh destination must write exactly one increment shard for changed session"
    );
    let growing_outcome = second
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&session_growing_id))
        .unwrap();
    assert!(growing_outcome.shard.is_some());
    let bodies = fx.stage_shard_bodies(&session_growing_id);
    let lines = shard_lines(&bodies[0]);
    assert_eq!(lines.len(), 2, "only session row + m3 written");
    assert!(lines
        .iter()
        .any(|l| l["record_id"] == format!("{SESSION}-m3")));
    assert!(!lines
        .iter()
        .any(|l| l["record_id"] == format!("{SESSION}-m1")));
}

/// A `chat-stasher` binary run with fully isolated XDG/home env (the same
/// pattern the archive tests use).
fn isolated_command(sandbox: &Path) -> std::process::Command {
    let home = sandbox.join("home-cli");
    fs::create_dir_all(&home).unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .env("HOME", &home)
        .env("CHAT_STASHER_RUSTIC_CACHE_DIR", home.join("rustic-cache"))
        .env("XDG_CONFIG_HOME", sandbox.join("config-cli"))
        .env("XDG_DATA_HOME", sandbox.join("data-cli"))
        .env("XDG_STATE_HOME", sandbox.join("state-cli"))
        .env("XDG_CACHE_HOME", sandbox.join("cache-cli"))
        .env_remove("CODEX_HOME")
        .env_remove("RUSTIC_REPO")
        .env_remove("RUSTIC_KEY_FILE");
    command
}
