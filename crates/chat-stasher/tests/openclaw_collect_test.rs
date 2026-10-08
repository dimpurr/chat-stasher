//! W873 · The openclaw collect wiring (TICKET-4D-10): what a read observed
//! firsthand has to travel beyond the sealed line, into the staged provenance
//! observation `collect_scan_report` appends. The read itself is covered by
//! `sqlite_probe`'s tests and the line's fold by `activity`'s; this file is
//! the third leg — the merge in `collect_scan_report` and the observation it
//! appends — which no other test drives.

#[path = "../src/test_support.rs"]
mod test_support;

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::id::SessionIdentity;
use chat_stasher::models::{HarnessSource, SessionRecord, SqliteSessionLayout};
use chat_stasher::provenance;
use chat_stasher::scanner::ScanReport;
use chat_stasher::sqlite_probe;
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;
use std::time::SystemTime;

/// One store in the shape `sqlite_probe`'s own tests read: a single agent,
/// one live window and one cold archive row, both carrying the store's own
/// `schema_meta.agent_id`.
fn openclaw_store(db: &Path) {
    let conn = Connection::open(db).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_meta(agent_id TEXT);
         INSERT INTO schema_meta VALUES ('synthetic-agent');
         CREATE TABLE session_windows(session_id TEXT, session_key TEXT, ended_at TEXT, started_at TEXT);
         INSERT INTO session_windows VALUES ('window-1', 'logical-1', NULL, '2026-01-02T03:04:05Z');
         CREATE TABLE transcript_events(session_id TEXT, seq INTEGER, event_json TEXT, created_at TEXT);
         INSERT INTO transcript_events VALUES ('window-1', 7,
           '{\"id\":\"event-7\",\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"provider\":\"synthetic-provider\",\"model\":\"synthetic-model\",\"usage\":{\"input\":11,\"output\":13,\"cacheRead\":17,\"cacheWrite\":19,\"totalTokens\":60,\"cost\":{\"total\":0.25}}}}',
           '2026-01-02T03:04:06Z');
         CREATE TABLE session_transcript_archives(session_id TEXT, generation INTEGER, session_key TEXT, reason TEXT, encoding TEXT, archive_blob BLOB, archive_sha256 TEXT, archive_name TEXT, created_at TEXT, published_at TEXT);
         INSERT INTO session_transcript_archives VALUES ('deleted-1', 2, 'logical-2', 'deleted', 'zstd', X'73796e7468657469632d636f6c642d61726368697665', 'cf3d6b9c3808b8ab261b086242d3e53e0d5b7d31eb2518319b679f4c3e4821c6', 'synthetic.jsonl.zst', '2026-01-03T00:00:00Z', '2026-01-03T00:00:01Z');",
    )
    .unwrap();
}

/// The record the scanner would push for one OpenClaw row of this store,
/// composed exactly the way `probe_openclaw_agents` composes it: the native
/// id is the hex-encoded agent/session pair, with `~g<generation>` on a cold
/// archive row.
fn record(db: &Path, machine: &str, native: String) -> SessionRecord {
    SessionRecord {
        id: SessionIdentity {
            source_short: "openclaw",
            machine: machine.to_string(),
            native_id: native,
        }
        .id(),
        absolute_path: db.to_path_buf(),
        byte_size: 0,
        mtime: SystemTime::now(),
        source: HarnessSource::OpenClaw,
        compressed: false,
        sqlite_layout: Some(SqliteSessionLayout::OpenClaw),
        provenance: Default::default(),
    }
}

fn scan_of(records: Vec<SessionRecord>) -> ScanReport {
    ScanReport {
        records,
        ..ScanReport::default()
    }
}

#[test]
fn collect_carries_openclaw_dimensions_into_the_provenance_observation() {
    let sandbox = test_support::Sandbox::new();
    let db = sandbox.root().join("openclaw-agent.sqlite");
    openclaw_store(&db);

    let machine = "synthetic-machine";
    let live = record(
        &db,
        machine,
        sqlite_probe::openclaw_native_id("synthetic-agent", "window-1"),
    );
    let cold = record(
        &db,
        machine,
        format!(
            "{}~g2",
            sqlite_probe::openclaw_native_id("synthetic-agent", "deleted-1")
        ),
    );
    let stage = sandbox.data_home().join("stage");
    let state = sandbox.state_home().join("chat-stasher");
    let destination = DestinationView::unreachable("synthetic-destination");

    collect::collect_scan_report(
        &scan_of(vec![live.clone(), cold.clone()]),
        &stage,
        machine,
        &state,
        20,
        &destination,
    )
    .unwrap();

    // The observation for a live window carries the store's own agent id and
    // no lifecycle status: "active" is never assumed from the window still
    // existing.
    let observations = provenance::read_observations(&stage, machine).unwrap();
    assert_eq!(observations.len(), 2, "one observation per session");
    let live_row = observations
        .iter()
        .find(|row| row.session_id == live.id)
        .expect("the live window session has an observation");
    let cold_row = observations
        .iter()
        .find(|row| row.session_id == cold.id)
        .expect("the cold archive session has an observation");
    assert_eq!(
        live_row.dimensions.container,
        ["synthetic-agent"],
        "the agent the store names itself is the container the session ran inside"
    );
    assert!(
        live_row.dimensions.status.is_empty(),
        "a live window records no lifecycle status, and none is assumed: {:?}",
        live_row.dimensions.status
    );
    assert!(live_row.dimensions.is_valid());

    // The observation for a cold archive row adds the one fact that row
    // states: the session is archived.
    assert_eq!(cold_row.session_id, cold.id);
    assert_eq!(cold_row.dimensions.container, ["synthetic-agent"]);
    assert_eq!(
        cold_row.dimensions.status,
        ["archived"],
        "a snapshot read from session_transcript_archives is the harness's own \
         record that the session is archived"
    );
    assert!(cold_row.dimensions.is_valid());

    // The sealed body the observation is a fact about is the read's own line:
    // what lands in the stage states the same facts the way the export
    // writes them.
    let live_body = chat_stasher::store::concat_shards(&stage, machine, &live.id).unwrap();
    let live_line: Value = serde_json::from_slice(&live_body).unwrap();
    assert_eq!(
        live_line["schema"],
        chat_stasher::sqlite_probe::OPENCLAW_SESSION_SCHEMA
    );
    assert_eq!(live_line["agent_id"], "synthetic-agent");
    let cold_body = chat_stasher::store::concat_shards(&stage, machine, &cold.id).unwrap();
    let cold_line: Value = serde_json::from_slice(&cold_body).unwrap();
    assert_eq!(
        cold_line["schema"],
        chat_stasher::sqlite_probe::OPENCLAW_ARCHIVE_SCHEMA
    );
    assert_eq!(cold_line["agent_id"], "synthetic-agent");

    // An unchanged source has nothing new to observe, so a second pass
    // appends nothing: the observations stay the two the first pass wrote.
    collect::collect_scan_report(
        &scan_of(vec![live, cold]),
        &stage,
        machine,
        &state,
        20,
        &destination,
    )
    .unwrap();
    let stable = provenance::read_observations(&stage, machine).unwrap();
    assert_eq!(
        stable.len(),
        2,
        "an unchanged store is not re-observed: {:?} observations",
        stable.len()
    );
}
