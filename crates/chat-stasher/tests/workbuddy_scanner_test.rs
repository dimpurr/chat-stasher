//! WorkBuddy dual-store source (W1046): the sqlite `sessions` table is the
//! metadata and tombstone half, the `*.jsonl` transcripts below `projects/`
//! are the speech half, and one session id is carried by both records into
//! one archive body.
//!
//! These tests pin the contract the ruling (CS-WB-2-GO) wrote: a row whose
//! `deleted_at` is not null exports a bare tombstone; a live row without a
//! matching transcript is a recognized session the archive could not hand
//! out — a named gap that turns `collect` partial (exit 3), never a zero;
//! the transcript glob accepts only `*.jsonl` and excludes the rollback and
//! tool-result neighbours by name; and the store is opened strictly
//! `mode=ro`, never `immutable=1`, so uncheckpointed WAL content still
//! reads — through a copied db/-wal/-shm trio when the store's own `-shm`
//! sidecar is absent.
//!
//! All fixtures are synthetic; no real conversation text, ids or names.

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::normalize::{normalize, Block};
use chat_stasher::{scanner, store};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

#[path = "../src/test_support.rs"]
mod test_support;

const USER_TURN: &str = r#"{"id":"synthetic-m1","timestamp":1780000000000,"type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder>synthetic injected context</system-reminder><user_query>synthetic genuine question</user_query>"}],"sessionId":"synthetic-session-id","cwd":"/synthetic/project"}"#;
const ASSISTANT_TURN: &str = r#"{"id":"synthetic-m2","timestamp":1780000000100,"type":"message","role":"assistant","content":[{"type":"output_text","text":"synthetic answer"}],"sessionId":"synthetic-session-id","cwd":"/synthetic/project"}"#;

fn synthetic_workbuddy_registry() -> scanner::HarnessRegistry {
    let cell = json!({
        "template": "~/.workbuddy/projects/",
        "format": "jsonl",
        "confidence": "measured-locally",
        "source": "synthetic fixture",
        "sql_table": "sessions",
        "sql_id_column": "id",
        "sql_required_columns": ["id", "transport", "deleted_at", "created_at", "last_activity_at"]
    });
    let paths = match scanner::current_platform() {
        "macos" => json!({"macos": cell}),
        "linux" => json!({"linux": cell}),
        "windows" => json!({"windows": cell}),
        platform => panic!("unexpected platform: {platform}"),
    };
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic fixture",
        "harnesses": [{"id": "workbuddy", "display_name": "WorkBuddy fixture", "paths": paths}]
    }))
    .unwrap()
}

/// One WorkBuddy layout below `~/.workbuddy`: a `workbuddy.db` whose
/// `sessions` table follows the measured schema (created empty unless rows
/// are added) plus, per session, an optional `<session-id>.jsonl` below
/// `projects/<project-slug>/`.
struct WorkBuddyFixture {
    sandbox: test_support::Sandbox,
    db: PathBuf,
}

impl WorkBuddyFixture {
    fn new() -> WorkBuddyFixture {
        let sandbox = test_support::Sandbox::new();
        fs::create_dir_all(sandbox.home().join(".workbuddy/projects")).unwrap();
        let db = sandbox.home().join(".workbuddy/workbuddy.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT,
                user_id TEXT,
                title TEXT,
                custom_title TEXT,
                status TEXT,
                mode TEXT,
                source_mode TEXT,
                model TEXT,
                transport TEXT,
                deleted_at INTEGER,
                created_at INTEGER,
                updated_at INTEGER,
                last_activity_at INTEGER
            );",
        )
        .unwrap();
        drop(conn);
        WorkBuddyFixture { sandbox, db }
    }

    /// A live local session row, the shape every measured installation so
    /// far writes for a real conversation.
    fn add_row(&self, session_id: &str) {
        self.add_row_with(session_id, None, Some("local"))
    }

    /// A session row with an explicit `deleted_at` (tombstone) and an
    /// explicit `transport`.
    fn add_row_with(&self, session_id: &str, deleted_at: Option<i64>, transport: Option<&str>) {
        let conn = rusqlite::Connection::open(&self.db).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, cwd, user_id, title, custom_title, status, mode, source_mode, model, transport, deleted_at, created_at, updated_at, last_activity_at)
             VALUES (?1, ?2, ?3, ?4, NULL, 'completed', 'craft', 'craft', 'synthetic-model', ?5, ?6, 1780000000000, 1780000000100, 1780000000200)",
            rusqlite::params![
                session_id,
                "/synthetic/project",
                "synthetic-user",
                "synthetic store title",
                transport,
                deleted_at
            ],
        )
        .unwrap();
    }

    fn write_transcript(&self, slug: &str, session_id: &str, body: &str) -> PathBuf {
        let path = self
            .sandbox
            .home()
            .join(format!(".workbuddy/projects/{slug}/{session_id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    fn scan(&self) -> scanner::ScanReport {
        // The registry template resolves through `~`, which in a test
        // process points at the machine's real home; the fixture states
        // the sandboxed projects root the way a user's own
        // `[harness_roots]` entry would (the grok fixture the same way).
        let config = Config {
            harness_roots: [(
                "workbuddy".to_string(),
                self.sandbox
                    .home()
                    .join(".workbuddy/projects")
                    .to_string_lossy()
                    .into(),
            )]
            .into_iter()
            .collect(),
            ..Config::default()
        };
        scanner::scan_with_registry_and_machine(
            &config,
            &synthetic_workbuddy_registry(),
            "synthetic-machine",
        )
        .unwrap()
    }

    fn collect_first(&self, scan: &scanner::ScanReport) -> PathBuf {
        let stage = self.sandbox.root().join("stage");
        let state = self.sandbox.root().join("state/collector.json");
        collect::collect_scan_report(
            scan,
            &stage,
            "synthetic-machine",
            &state,
            20,
            &DestinationView::unreachable("synthetic-destination"),
        )
        .unwrap();
        stage
    }

    fn collect_again(
        &self,
        stage: &std::path::Path,
        scan: &scanner::ScanReport,
    ) -> collect::CollectReport {
        let state = self.sandbox.root().join("state/collector.json");
        collect::collect_scan_report(
            scan,
            stage,
            "synthetic-machine",
            &state,
            20,
            &DestinationView::unreachable("synthetic-destination"),
        )
        .unwrap()
    }
}

fn body_string(stage: &std::path::Path, id: &str) -> String {
    let bytes = store::concat_shards(stage, "synthetic-machine", id).unwrap();
    String::from_utf8(bytes).expect("workbuddy body is valid UTF-8")
}

fn session_id(native: &str) -> String {
    format!("workbuddy.synthetic-machine.{native}")
}

#[test]
fn a_paired_session_becomes_two_records_under_one_id_and_no_gap() {
    let fixture = WorkBuddyFixture::new();
    fixture.add_row("synthetic-session-001");
    let transcript = fixture.write_transcript(
        "synthetic-slug",
        "synthetic-session-001",
        &format!("{USER_TURN}\n{ASSISTANT_TURN}\n"),
    );

    let scan = fixture.scan();
    let id = session_id("synthetic-session-001");
    let records: Vec<_> = scan
        .records
        .iter()
        .filter(|record| record.id == id)
        .collect();
    assert_eq!(records.len(), 2, "the row and the transcript share one id");
    let row_record = records
        .iter()
        .find(|record| record.sqlite_layout.is_some())
        .expect("the db row carries a virtual sqlite record");
    assert_eq!(
        row_record.absolute_path,
        fixture.sandbox.home().join(".workbuddy/workbuddy.db")
    );
    let file_record = records
        .iter()
        .find(|record| record.sqlite_layout.is_none())
        .expect("the transcript is an ordinary file record");
    assert_eq!(file_record.absolute_path, transcript);

    let probe = scan.probes.iter().find(|p| p.id == "workbuddy").unwrap();
    assert!(format!("{:?}", probe.state).contains("Scanned"));
    assert_eq!(probe.record_count, Some(1), "one recognized session");
    assert_eq!(probe.unreadable_count, Some(0));
    assert!(probe.note.contains("tombstoned=0"));
    assert!(
        scan.archive_gaps().is_empty(),
        "everything recognized was archived"
    );
}

#[test]
fn a_deleted_row_exports_a_tombstone_record_without_its_payload() {
    let fixture = WorkBuddyFixture::new();
    // A tombstone whose transcript is already gone from disk, exactly the
    // case the purge tool leaves behind.
    fixture.add_row_with("synthetic-session-002", Some(1780000123400), None);

    let scan = fixture.scan();
    let id = session_id("synthetic-session-002");
    let records: Vec<_> = scan
        .records
        .iter()
        .filter(|record| record.id == id)
        .collect();
    assert_eq!(
        records.len(),
        1,
        "the tombstone is the session's one record, and it is the row"
    );
    assert!(records[0].sqlite_layout.is_some());

    let probe = scan.probes.iter().find(|p| p.id == "workbuddy").unwrap();
    assert_eq!(probe.record_count, Some(1));
    assert!(probe.note.contains("tombstoned=1"));
    assert!(
        scan.archive_gaps().is_empty(),
        "a session the source deleted is recorded, not a gap"
    );

    let stage = fixture.collect_first(&scan);
    let body = body_string(&stage, &id);
    let tombstone: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(
        tombstone["schema"], "chat-stasher.workbuddy.session.v1",
        "the raw body keeps the tombstone line"
    );
    assert_eq!(tombstone["session"], "synthetic-session-002");
    assert_eq!(tombstone["deleted"], true);
    assert_eq!(
        tombstone.as_object().unwrap().len(),
        3,
        "the tombstone carries the identity and the deletion marker, no row payload"
    );
    assert!(
        !body.contains("synthetic store title"),
        "a tombstone never archives the deleted session's metadata"
    );

    // The tombstone is sealed exactly once: its bytes cannot change.
    let scan = fixture.scan();
    let report = fixture.collect_again(&stage, &scan);
    assert_eq!(report.changed_records, 0, "the quiet pass seals nothing");
    assert_eq!(body_string(&stage, &id), body);
}

#[test]
fn a_live_row_without_a_transcript_is_a_named_gap_and_never_a_zero() {
    let fixture = WorkBuddyFixture::new();
    // The unknown arm: a live row this machine holds no transcript bytes
    // for — whether its transport never writes one locally, or the jsonl a
    // project move left behind is gone.
    fixture.add_row_with("synthetic-session-003", None, Some("local"));
    // An orphan transcript from the same store: recognized and archived on
    // its own id, and never allowed to mask the row's gap.
    fixture.write_transcript(
        "synthetic-slug",
        "synthetic-session-orphan",
        &format!("{USER_TURN}\n"),
    );

    let scan = fixture.scan();
    let gaps = scan.archive_gaps();
    assert_eq!(gaps.len(), 1, "collect is partial: exit 3, never 0");
    assert_eq!(gaps[0].harness_id, "workbuddy");
    assert_eq!(gaps[0].recognized_sessions, Some(2));
    assert_eq!(
        gaps[0].session_records, 1,
        "only the orphan's id was emitted; the row's session was not"
    );

    let probe = scan.probes.iter().find(|p| p.id == "workbuddy").unwrap();
    assert_eq!(probe.record_count, Some(2), "rows plus orphan transcripts");
    assert_eq!(probe.unreadable_count, Some(1));
    assert!(probe
        .note
        .contains("live rows without a local transcript jsonl=1"));
    assert!(
        scan.records
            .iter()
            .any(|record| record.id == session_id("synthetic-session-orphan")),
        "the orphan transcript still archives"
    );
    assert!(
        !scan
            .records
            .iter()
            .any(|record| record.id == session_id("synthetic-session-003")),
        "no record exists for the gap session"
    );

    let stage = fixture.collect_first(&scan);
    let orphan_body = body_string(&stage, &session_id("synthetic-session-orphan"));
    assert!(orphan_body.contains("synthetic genuine question"));
}

#[test]
fn the_glob_takes_only_jsonl_and_excludes_the_rollback_and_tool_results_neighbours() {
    let fixture = WorkBuddyFixture::new();
    fixture.add_row("synthetic-session-004");
    let transcript = fixture.write_transcript(
        "synthetic-slug",
        "synthetic-session-004",
        &format!("{USER_TURN}\n"),
    );
    let rollback = fixture
        .sandbox
        .home()
        .join(".workbuddy/projects/synthetic-slug/state.file-rollback.ndjson");
    fs::write(&rollback, r#"{"ca":"synthetic","commitSeq":1}"#).unwrap();
    let tool_results = fixture
        .sandbox
        .home()
        .join(".workbuddy/projects/synthetic-slug/tool-results");
    fs::create_dir_all(&tool_results).unwrap();
    fs::write(
        tool_results.join("synthetic-render.json"),
        br#"{"renderer":"text"}"#,
    )
    .unwrap();
    let stray = fixture
        .sandbox
        .home()
        .join(".workbuddy/projects/synthetic-slug/synthetic-notes.md");
    fs::write(&stray, b"synthetic not a transcript").unwrap();

    let scan = fixture.scan();
    let id = session_id("synthetic-session-004");
    let records: Vec<_> = scan
        .records
        .iter()
        .filter(|record| record.id == id)
        .collect();
    assert_eq!(records.len(), 2, "the session's two halves");
    assert_eq!(
        records
            .iter()
            .find(|record| record.sqlite_layout.is_none())
            .expect("the transcript file record")
            .absolute_path,
        transcript
    );
    assert!(
        scan.records
            .iter()
            .all(|record| record.absolute_path != rollback),
        "rollback journals are excluded by name"
    );
    assert!(
        scan.records
            .iter()
            .all(|record| !record.absolute_path.starts_with(&tool_results)),
        "the tool-results cache is excluded by name"
    );
    assert!(
        scan.records
            .iter()
            .all(|record| record.absolute_path != stray),
        "non-jsonl files are not transcripts"
    );
}

#[test]
fn duplicate_transcript_copies_keep_the_newest_and_name_the_stale_ones() {
    let fixture = WorkBuddyFixture::new();
    fixture.add_row("synthetic-session-005");
    let stale = fixture.write_transcript(
        "synthetic-old-slug",
        "synthetic-session-005",
        &format!("{USER_TURN}\n"),
    );
    let fresh = fixture.write_transcript(
        "synthetic-new-slug",
        "synthetic-session-005",
        &format!("{USER_TURN}\n{ASSISTANT_TURN}\n"),
    );
    // The newer copy wins: backdate the stale one past the fresh one.
    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(old_time)
        .unwrap();

    let scan = fixture.scan();
    let id = session_id("synthetic-session-005");
    let transcripts: Vec<_> = scan
        .records
        .iter()
        .filter(|record| record.id == id && record.sqlite_layout.is_none())
        .collect();
    assert_eq!(transcripts.len(), 1, "one archive slot answers for one id");
    assert_eq!(transcripts[0].absolute_path, fresh);
    let probe = scan.probes.iter().find(|p| p.id == "workbuddy").unwrap();
    assert!(probe.note.contains("stale duplicates skipped=1"));
}

#[test]
fn a_missing_store_keeps_the_transcripts_and_the_count_stays_unknown() {
    let fixture = WorkBuddyFixture::new();
    fs::remove_file(&fixture.db).unwrap();
    fixture.write_transcript(
        "synthetic-slug",
        "synthetic-session-006",
        &format!("{USER_TURN}\n"),
    );

    let scan = fixture.scan();
    let probe = scan.probes.iter().find(|p| p.id == "workbuddy").unwrap();
    assert_eq!(
        probe.record_count,
        Some(1),
        "the transcript is the recognized session"
    );
    assert!(probe.note.contains("workbuddy.db is not present"));
    assert!(probe.note.contains("unknown, not 0"));
    assert_eq!(
        scan.records
            .iter()
            .filter(|record| record.id == session_id("synthetic-session-006"))
            .count(),
        1
    );
    assert!(scan.archive_gaps().is_empty());
}

#[test]
fn an_unreadable_schema_keeps_the_transcripts_and_reports_nothing_as_zero() {
    let fixture = WorkBuddyFixture::new();
    // A store whose sessions table predates the transport column: the
    // schema this build was measured against is gone, so the count is the
    // error's, never a fabricated one.
    let conn = rusqlite::Connection::open(&fixture.db).unwrap();
    conn.execute_batch(
        "DROP TABLE sessions; CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT);",
    )
    .unwrap();
    drop(conn);
    fixture.write_transcript(
        "synthetic-slug",
        "synthetic-session-007",
        &format!("{USER_TURN}\n"),
    );

    let scan = fixture.scan();
    let probe = scan.probes.iter().find(|p| p.id == "workbuddy").unwrap();
    assert_eq!(
        probe.record_count, None,
        "a store that cannot be enumerated keeps an unknown count"
    );
    assert!(probe.note.contains("WorkBuddy schema mismatch"));
    assert!(probe.note.contains("session count unknown, not 0"));
    assert_eq!(
        scan.records
            .iter()
            .filter(|record| record.id == session_id("synthetic-session-007"))
            .count(),
        1,
        "the transcript still archives"
    );
    assert!(scan.archive_gaps().is_empty());
}

#[test]
fn one_session_body_holds_the_metadata_line_and_the_whole_transcript() {
    let fixture = WorkBuddyFixture::new();
    let id = session_id("synthetic-session-101");
    fixture.add_row("synthetic-session-101");
    fixture.write_transcript(
        "synthetic-slug",
        "synthetic-session-101",
        &format!("{USER_TURN}\n{ASSISTANT_TURN}\n"),
    );

    let scan = fixture.scan();
    let stage = fixture.collect_first(&scan);
    let body = body_string(&stage, &id);
    assert_eq!(body.lines().count(), 3, "one metadata line, two turns");
    assert!(body.contains(r#""schema":"chat-stasher.workbuddy.session.v1""#));
    assert!(body.contains("synthetic store title"));
    assert!(body.contains("synthetic genuine question"));
    assert!(body.contains("synthetic answer"));

    // The derived reading is one conversation: the metadata is bookkeeping,
    // the transcript is the speech.
    let conversation = normalize("workbuddy", &body);
    let rendered: Vec<String> = conversation
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            Block::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        rendered,
        vec!["synthetic genuine question", "synthetic answer"]
    );
    assert!(!format!("{conversation:?}").contains("synthetic injected context"));

    // A quiet pass stages nothing.
    let scan = fixture.scan();
    let report = fixture.collect_again(&stage, &scan);
    assert_eq!(report.changed_records, 0);
    assert_eq!(body_string(&stage, &id), body);
}

#[test]
fn a_row_edit_re_exports_once_and_a_later_deletion_appends_its_tombstone() {
    let fixture = WorkBuddyFixture::new();
    let id = session_id("synthetic-session-102");
    fixture.add_row("synthetic-session-102");
    fixture.write_transcript(
        "synthetic-slug",
        "synthetic-session-102",
        &format!("{USER_TURN}\n"),
    );

    let scan = fixture.scan();
    let stage = fixture.collect_first(&scan);
    let body_before = body_string(&stage, &id);
    assert_eq!(body_before.lines().count(), 2);

    // The row's own metadata changes: exactly one re-exported line, and the
    // transcript cursor is untouched by it.
    let conn = rusqlite::Connection::open(&fixture.db).unwrap();
    conn.execute(
        "UPDATE sessions SET title = 'synthetic edited title' WHERE id = 'synthetic-session-102'",
        [],
    )
    .unwrap();
    drop(conn);
    let scan = fixture.scan();
    let report = fixture.collect_again(&stage, &scan);
    let body_after = body_string(&stage, &id);
    assert!(body_after.starts_with(&body_before), "the body only grows");
    assert_eq!(
        body_after.lines().count(),
        3,
        "one re-exported metadata line"
    );
    assert_eq!(body_after.matches("synthetic edited title").count(), 1);
    assert_eq!(report.changed_records, 1);

    // The source then soft-deletes the session: the tombstone appends, and
    // the transcript bytes stay exactly where they were.
    let conn = rusqlite::Connection::open(&fixture.db).unwrap();
    conn.execute(
        "UPDATE sessions SET deleted_at = 1780000999999 WHERE id = 'synthetic-session-102'",
        [],
    )
    .unwrap();
    drop(conn);
    let scan = fixture.scan();
    let report = fixture.collect_again(&stage, &scan);
    let body_tombstoned = body_string(&stage, &id);
    assert!(body_tombstoned.starts_with(&body_after));
    assert_eq!(
        body_tombstoned.lines().count(),
        4,
        "the tombstone is one more line"
    );
    let appended_tombstone: serde_json::Value = serde_json::from_str(
        body_tombstoned
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .expect("the tombstone line"),
    )
    .unwrap();
    assert_eq!(
        appended_tombstone["schema"],
        "chat-stasher.workbuddy.session.v1"
    );
    assert_eq!(appended_tombstone["session"], "synthetic-session-102");
    assert_eq!(appended_tombstone["deleted"], true);
    assert_eq!(
        appended_tombstone.as_object().unwrap().len(),
        3,
        "the tombstone carries no row payload"
    );
    assert_eq!(report.changed_records, 1);
    assert_eq!(
        body_tombstoned.matches(USER_TURN).count(),
        1,
        "the transcript bytes are sealed exactly once"
    );
}

#[test]
fn wal_content_without_an_shm_sidecar_reads_through_a_copied_trio_never_immutable() {
    let fixture = WorkBuddyFixture::new();
    // Build a live WAL store: the connection stays open, so the committed
    // rows live in the -wal and the -shm exists beside it.
    let conn = rusqlite::Connection::open(&fixture.db).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    conn.execute(
        "INSERT INTO sessions (id, transport, deleted_at, created_at, last_activity_at, title) VALUES ('synthetic-session-201', 'local', NULL, 1780000000000, 1780000000000, 'synthetic wal title')",
        [],
    )
    .unwrap();
    // Copy the db and the -wal while the writer is live; the -shm is a
    // private shared-memory file and is deliberately not copied, so the
    // copy is the exact shape a copied dead store has: WAL content, no -shm.
    let copied_dir = fixture.sandbox.root().join("copied-store");
    fs::create_dir_all(&copied_dir).unwrap();
    fs::copy(&fixture.db, copied_dir.join("workbuddy.db")).unwrap();
    fs::copy(
        fixture.sandbox.home().join(".workbuddy/workbuddy.db-wal"),
        copied_dir.join("workbuddy.db-wal"),
    )
    .unwrap();
    let copied_db = copied_dir.join("workbuddy.db");
    drop(conn);

    let rows = chat_stasher::sqlite_probe::enumerate_workbuddy_sessions(&copied_db)
        .expect("the copy reads");
    assert_eq!(rows.len(), 1, "the uncheckpointed WAL content was read");
    assert_eq!(rows[0].id, "synthetic-session-201");
    assert_eq!(rows[0].transport.as_deref(), Some("local"));
    assert!(!rows[0].deleted);
    assert_eq!(rows[0].time_ms, Some(1780000000000));
    // The read never created sidecars beside the user's own store copy.
    assert!(
        !copied_dir.join("workbuddy.db-shm").exists(),
        "the original copy's directory is byte-unchanged; the read used its own trio"
    );
}
