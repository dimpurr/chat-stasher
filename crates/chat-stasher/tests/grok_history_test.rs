//! Grok CLI native transcripts (`chat_history.jsonl`) are archived into the
//! session's own body, next to the `session_docs` export.
//!
//! The reader (GC-1) makes one body of both observations: a body whose native
//! records render at least one message reads as one conversation — the native
//! turns — and must not append the legacy plain-text summary again, while the
//! raw body keeps both. These tests pin the collector's half of that contract:
//! every `chat_history.jsonl` reaches the archive, including sessions the
//! `session_docs` store has no row for, each file keeps an ordinary
//! append-only cursor, and no existing archived id changes. The interleavings
//! a first collect cannot see get their own tests: a row edited between
//! transcript appends, an export-only stage that gains its transcript later,
//! and the byte-identity of every id the pre-native scanner already produced.
//!
//! All fixtures are synthetic; no real conversation text, ids or names.

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::normalize::{normalize, Block};
use chat_stasher::{scanner, store};
use serde_json::json;
use std::collections::BTreeSet;
use std::fs;

#[path = "../src/test_support.rs"]
mod test_support;

const NATIVE_TURN_1: &str =
    r#"{"type":"user","content":[{"type":"text","text":"synthetic user turn"}]}"#;
const NATIVE_TURN_2: &str = r#"{"type":"assistant","content":"synthetic answer"}"#;
const NATIVE_TURN_3: &str = r#"{"type":"assistant","content":"synthetic appended answer"}"#;

fn synthetic_grok_registry() -> scanner::HarnessRegistry {
    let cell = json!({
        "template": "~/.grok/sessions/session_search.sqlite",
        "format": "sqlite",
        "confidence": "measured-locally",
        "source": "synthetic fixture",
        "sql_table": "session_docs",
        "sql_id_column": "session_id",
        "sql_required_columns": ["session_id", "updated_at"],
        "sql_time_column": "updated_at",
        "sql_time_value_is_seconds": true
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
        "harnesses": [{"id": "grok", "display_name": "Grok CLI fixture", "paths": paths}]
    }))
    .unwrap()
}

/// One grok store under `~/.grok/sessions` whose `session_docs` table exists
/// (empty unless rows are added) plus, per session, an optional
/// `chat_history.jsonl` under `sessions/<sanitized-cwd>/<session-id>/`.
struct GrokFixture {
    sandbox: test_support::Sandbox,
    db: std::path::PathBuf,
}

impl GrokFixture {
    fn new() -> GrokFixture {
        let sandbox = test_support::Sandbox::new();
        let sessions = sandbox.home().join(".grok/sessions");
        fs::create_dir_all(&sessions).unwrap();
        let db = sessions.join("session_search.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL, updated_at INTEGER NOT NULL, title TEXT NOT NULL, content TEXT NOT NULL, content_hash TEXT NOT NULL);",
        )
        .unwrap();
        drop(conn);
        GrokFixture { sandbox, db }
    }

    fn add_row(&self, session_id: &str, updated_at: i64, content: &str) {
        let conn = rusqlite::Connection::open(&self.db).unwrap();
        conn.execute(
            "INSERT INTO session_docs VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                session_id,
                "/synthetic/work",
                updated_at,
                "synthetic title",
                content,
                "synthetic hash"
            ],
        )
        .unwrap();
    }

    fn update_row(&self, session_id: &str, updated_at: i64, content: &str) {
        let conn = rusqlite::Connection::open(&self.db).unwrap();
        let updated = conn
            .execute(
                "UPDATE session_docs SET content = ?1, updated_at = ?2 WHERE session_id = ?3",
                rusqlite::params![content, updated_at, session_id],
            )
            .unwrap();
        assert_eq!(
            updated, 1,
            "the fixture must edit exactly the row under test"
        );
    }

    fn write_history(&self, session_id: &str, body: &str) -> std::path::PathBuf {
        let path = self
            .sandbox
            .home()
            .join(".grok/sessions/synthetic-cwd")
            .join(session_id)
            .join("chat_history.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    fn write_usage(&self, session_id: &str) {
        let path = self
            .sandbox
            .home()
            .join(".grok/sessions/synthetic-cwd")
            .join(session_id)
            .join("usage.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"modelUsage":{"grok-test":{"inputTokens":12,"cachedReadTokens":7,"outputTokens":3,"totalTokens":15}}}"#,
        )
        .unwrap();
    }

    fn scan(&self) -> scanner::ScanReport {
        let config = Config {
            harness_roots: [("grok".to_string(), self.db.to_string_lossy().into())]
                .into_iter()
                .collect(),
            ..Config::default()
        };
        scanner::scan_with_registry_and_machine(
            &config,
            &synthetic_grok_registry(),
            "synthetic-machine",
        )
        .unwrap()
    }

    fn collect_first(&self, scan: &scanner::ScanReport) -> std::path::PathBuf {
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
    String::from_utf8(bytes).expect("grok body is valid UTF-8")
}

fn rendered_texts(body: &str) -> Vec<String> {
    let conversation = normalize("grok", body);
    conversation
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            Block::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn history_is_archived_into_the_row_session_body_and_reads_as_one_conversation() {
    let fixture = GrokFixture::new();
    fixture.add_row(
        "synthetic-session-001",
        1_780_000_000,
        "synthetic legacy summary",
    );
    let history = fixture.write_history(
        "synthetic-session-001",
        &format!("{NATIVE_TURN_1}\n{NATIVE_TURN_2}\n"),
    );
    fixture.write_usage("synthetic-session-001");

    let scan = fixture.scan();
    let sqlite_id = "grok.synthetic-machine.synthetic-session-001";

    // The existing ids are unchanged: one row record, one `.usage` sidecar.
    assert_eq!(
        scan.records
            .iter()
            .filter(|record| record.id == sqlite_id && record.sqlite_layout.is_some())
            .count(),
        1,
        "the session_docs row keeps its own record"
    );
    let usage = scan
        .records
        .iter()
        .find(|record| record.id == format!("{sqlite_id}.usage"))
        .expect("usage.json keeps its own linked id");
    assert_eq!(usage.sqlite_layout, None);

    // The native transcript is carried by a file record under the SAME id,
    // so the two observations of one session seal into one body.
    let history_records: Vec<_> = scan
        .records
        .iter()
        .filter(|record| record.id == sqlite_id && record.sqlite_layout.is_none())
        .collect();
    assert_eq!(
        history_records.len(),
        1,
        "one chat_history.jsonl record shares the session_docs row's id"
    );
    assert_eq!(history_records[0].absolute_path, history);
    assert!(history_records[0]
        .absolute_path
        .ends_with("chat_history.jsonl"));

    let stage = fixture.collect_first(&scan);
    let body = body_string(&stage, sqlite_id);

    // Raw/export keeps both observations of the session.
    assert!(
        body.contains(r#""schema":"chat-stasher.sqlite.session.v1""#),
        "the raw body keeps the legacy export line"
    );
    assert!(body.contains("synthetic legacy summary"));
    assert!(body.contains(NATIVE_TURN_1));
    assert!(body.contains(NATIVE_TURN_2));

    // Derived reading is ONE conversation: native turns, no second copy of
    // the legacy summary as another turn (GC-1's per-body suppression).
    let rendered = rendered_texts(&body);
    assert_eq!(rendered, vec!["synthetic user turn", "synthetic answer"]);

    // The usage sidecar is untouched by the new producer.
    let usage_body = body_string(&stage, "grok.synthetic-machine.synthetic-session-001.usage");
    assert!(usage_body.contains("modelUsage"));
}

#[test]
fn a_session_without_a_row_is_archived_under_its_own_session_id() {
    let fixture = GrokFixture::new();
    fixture.write_history(
        "synthetic-session-002",
        &format!("{NATIVE_TURN_1}\n{NATIVE_TURN_2}\n"),
    );

    let scan = fixture.scan();
    let native_id = "grok.synthetic-machine.synthetic-session-002";
    let records: Vec<_> = scan
        .records
        .iter()
        .filter(|record| record.id == native_id)
        .collect();
    assert_eq!(
        records.len(),
        1,
        "a native-only session yields exactly one record"
    );
    assert_eq!(records[0].sqlite_layout, None);
    assert!(records[0].absolute_path.ends_with("chat_history.jsonl"));

    let stage = fixture.collect_first(&scan);
    let body = body_string(&stage, native_id);
    assert!(body.contains(NATIVE_TURN_1));
    assert!(body.contains(NATIVE_TURN_2));
    assert!(
        !body.contains("chat-stasher.sqlite.session.v1"),
        "no row exists, so the body holds the native transcript alone"
    );

    let rendered = rendered_texts(&body);
    assert_eq!(rendered, vec!["synthetic user turn", "synthetic answer"]);
}

#[test]
fn a_row_session_without_a_history_file_archives_only_the_export() {
    let fixture = GrokFixture::new();
    fixture.add_row(
        "synthetic-session-003",
        1_780_000_000,
        "synthetic legacy summary",
    );

    let scan = fixture.scan();
    let sqlite_id = "grok.synthetic-machine.synthetic-session-003";
    let records: Vec<_> = scan
        .records
        .iter()
        .filter(|record| record.id == sqlite_id)
        .collect();
    assert_eq!(
        records.len(),
        1,
        "with no chat_history.jsonl on disk, the row is the session's only record"
    );
    assert!(records[0].sqlite_layout.is_some());

    let stage = fixture.collect_first(&scan);
    let body = body_string(&stage, sqlite_id);
    assert!(body.contains("synthetic legacy summary"));
    assert!(!body.contains(NATIVE_TURN_1));
    // A legacy-only body still reads its summary: the summary is the only
    // readable text the body holds.
    let rendered = rendered_texts(&body);
    assert_eq!(rendered, vec!["synthetic legacy summary"]);
}

#[test]
fn appending_to_chat_history_archives_only_the_new_lines() {
    let fixture = GrokFixture::new();
    fixture.add_row(
        "synthetic-session-004",
        1_780_000_000,
        "synthetic legacy summary",
    );
    let two_turns = format!("{NATIVE_TURN_1}\n{NATIVE_TURN_2}\n");
    let history = fixture.write_history("synthetic-session-004", &two_turns);

    let scan = fixture.scan();
    let stage = fixture.collect_first(&scan);
    let sqlite_id = "grok.synthetic-machine.synthetic-session-004";
    let body_before = body_string(&stage, sqlite_id);
    assert_eq!(
        body_before.lines().count(),
        3,
        "export line plus two native lines"
    );

    // The session continues: only the transcript file grows; the store row is
    // untouched, so a correct collector must add exactly the new bytes.
    let mut appended = two_turns.clone();
    appended.push_str(NATIVE_TURN_3);
    appended.push('\n');
    fs::write(&history, appended).unwrap();

    let scan = fixture.scan();
    fixture.collect_again(&stage, &scan);
    let body_after = body_string(&stage, sqlite_id);
    assert_eq!(body_after.lines().count(), 4, "one line was appended");
    assert_eq!(body_after.matches(NATIVE_TURN_1).count(), 1);
    assert_eq!(body_after.matches(NATIVE_TURN_2).count(), 1);
    assert_eq!(body_after.matches(NATIVE_TURN_3).count(), 1);
    assert_eq!(
        body_after.matches("synthetic legacy summary").count(),
        1,
        "the export line is still sealed exactly once"
    );

    // The derived reading gains the appended turn and keeps its shape.
    let rendered = rendered_texts(&body_after);
    assert_eq!(
        rendered,
        vec![
            "synthetic user turn",
            "synthetic answer",
            "synthetic appended answer"
        ]
    );

    // A second quiet pass seals nothing.
    let scan = fixture.scan();
    fixture.collect_again(&stage, &scan);
    let body_quiet = body_string(&stage, sqlite_id);
    assert_eq!(
        body_quiet.lines().count(),
        4,
        "an unchanged transcript appends nothing"
    );
    assert_eq!(body_quiet, body_after);
}

/// Row-update-after-history interleave: the session advances on both
/// producers between two passes. The store row is edited (new content, new
/// `updated_at`), so the snapshot path re-exports it, while the native
/// transcript appends one turn past its byte cursor. Each producer keeps its
/// own cursor into the one body, so the interleave must seal each new
/// observation exactly once and rewrite nothing that is already staged.
///
/// The snapshot re-export is the pass's one reported cursor reset — a SQLite
/// session is re-read from the store, it has no byte cursor to continue —
/// which is a statement about the read, not about the archive. What must
/// never reset is the body itself: the assertions below pin that the pass
/// extends the staged bytes rather than sealing a fresh whole-session copy
/// over them.
#[test]
fn a_row_update_between_history_appends_is_a_pure_extension() {
    let fixture = GrokFixture::new();
    let sqlite_id = "grok.synthetic-machine.synthetic-session-007";
    fixture.add_row(
        "synthetic-session-007",
        1_780_000_000,
        "synthetic legacy summary one",
    );
    let two_turns = format!("{NATIVE_TURN_1}\n{NATIVE_TURN_2}\n");
    fixture.write_history("synthetic-session-007", &two_turns);

    let scan = fixture.scan();
    let stage = fixture.collect_first(&scan);
    let body_before = body_string(&stage, sqlite_id);
    assert_eq!(
        body_before.lines().count(),
        3,
        "one export line and two native lines"
    );

    // Both producers move: the row is edited and the transcript grows.
    fixture.update_row(
        "synthetic-session-007",
        1_780_000_600,
        "synthetic legacy summary two",
    );
    let mut three_turns = two_turns;
    three_turns.push_str(NATIVE_TURN_3);
    three_turns.push('\n');
    fixture.write_history("synthetic-session-007", &three_turns);

    let scan = fixture.scan();
    let report = fixture.collect_again(&stage, &scan);
    let body_after = body_string(&stage, sqlite_id);

    // Append-only: the body this pass started from is still its prefix, so
    // neither producer rewrote the body from zero.
    assert!(
        body_after.starts_with(&body_before),
        "the pass extends the staged body; it never resets or rewrites it"
    );
    assert_eq!(
        body_after.lines().count(),
        5,
        "one re-exported row line and one appended native turn"
    );
    // Nothing duplicated, nothing lost: every observation exactly once.
    assert_eq!(
        body_after.matches("synthetic legacy summary one").count(),
        1,
        "the first export is still sealed exactly once"
    );
    assert_eq!(
        body_after.matches("synthetic legacy summary two").count(),
        1,
        "the edited row is re-exported exactly once"
    );
    assert_eq!(body_after.matches(NATIVE_TURN_1).count(), 1);
    assert_eq!(body_after.matches(NATIVE_TURN_2).count(), 1);
    assert_eq!(body_after.matches(NATIVE_TURN_3).count(), 1);
    assert_eq!(
        report.changed_records, 2,
        "each producer staged exactly its new observation"
    );
    assert_eq!(
        report.lines_written, 2,
        "exactly the re-exported row and the appended turn were written"
    );
    assert_eq!(report.shards_written, 2);
    assert_eq!(
        report.reset_records, 1,
        "only the row's snapshot re-read reports a reset; the transcript's \
         cursor append is not one"
    );

    // Derived reading stays one conversation: the native turns, with neither
    // legacy summary appended as another turn.
    let rendered = rendered_texts(&body_after);
    assert_eq!(
        rendered,
        vec![
            "synthetic user turn",
            "synthetic answer",
            "synthetic appended answer"
        ],
        "derived reading keeps the native turns and appends neither summary"
    );

    // A pass in which neither producer moved stages nothing at all: the
    // interleave left both cursors at positions the next pass honors.
    let scan = fixture.scan();
    let report = fixture.collect_again(&stage, &scan);
    assert_eq!(
        report.changed_records, 0,
        "an unchanged row and an unchanged transcript stage nothing"
    );
    let body_quiet = body_string(&stage, sqlite_id);
    assert_eq!(body_quiet, body_after, "the quiet pass wrote nothing");
}

/// The legacy upgrade: a session the pre-native archive already holds as an
/// export-only body — the only shape a collect before the native
/// `chat_history.jsonl` collection could produce — gains its transcript
/// later. The upgraded build must extend that body in place: adopt the row's
/// untouched observation, reset nothing, re-seal nothing, and switch the
/// derived reading from the legacy summary to the native turns exactly as a
/// body collected from scratch would.
#[test]
fn an_export_only_stage_gains_its_history_file_as_a_pure_extension() {
    let fixture = GrokFixture::new();
    let sqlite_id = "grok.synthetic-machine.synthetic-session-008";
    fixture.add_row(
        "synthetic-session-008",
        1_780_000_000,
        "synthetic legacy summary",
    );

    // No transcript exists yet, so the first pass stages the session the way
    // every pre-native collect did: the row's export line alone.
    let scan = fixture.scan();
    let stage = fixture.collect_first(&scan);
    let body_before = body_string(&stage, sqlite_id);
    assert_eq!(
        body_before.lines().count(),
        1,
        "the export line is the whole body"
    );
    assert_eq!(
        rendered_texts(&body_before),
        vec!["synthetic legacy summary"]
    );

    // The CLI later writes the session's native transcript.
    fixture.write_history(
        "synthetic-session-008",
        &format!("{NATIVE_TURN_1}\n{NATIVE_TURN_2}\n"),
    );

    let scan = fixture.scan();
    let report = fixture.collect_again(&stage, &scan);
    let body_after = body_string(&stage, sqlite_id);

    // Pure extension: the export-only body is still the prefix of the body,
    // so the upgrade neither reset the stage nor rewrote it from zero.
    assert!(
        body_after.starts_with(&body_before),
        "the upgrade extends the export-only body in place"
    );
    assert_eq!(
        body_after.lines().count(),
        3,
        "the untouched export plus both native turns"
    );
    assert_eq!(
        body_after.matches("synthetic legacy summary").count(),
        1,
        "the already staged export was not sealed a second time"
    );
    assert_eq!(body_after.matches(NATIVE_TURN_1).count(), 1);
    assert_eq!(body_after.matches(NATIVE_TURN_2).count(), 1);
    assert_eq!(
        report.changed_records, 1,
        "only the new transcript staged anything; the row was re-observed \
         unchanged, not re-sealed"
    );
    assert_eq!(
        report.lines_written, 2,
        "the transcript's first capture reads the file whole"
    );
    assert_eq!(
        report.reset_records, 0,
        "an upgrade pass resets no record's cursor"
    );

    let rendered = rendered_texts(&body_after);
    assert_eq!(
        rendered,
        vec!["synthetic user turn", "synthetic answer"],
        "derived reading prefers the native turns over the legacy summary"
    );

    // The next quiet pass proves the first capture left both producers — the
    // unchanged row and the now fully archived transcript — at rest.
    let scan = fixture.scan();
    let report = fixture.collect_again(&stage, &scan);
    assert_eq!(report.changed_records, 0);
    let body_quiet = body_string(&stage, sqlite_id);
    assert_eq!(body_quiet, body_after, "the quiet pass wrote nothing");
}

/// Seed the census tree: one session per population the two scanners can
/// disagree about — row-only, row plus transcript plus usage, native-only
/// (whose `usage.json` must stay unrecorded under the sidecar collector's
/// known-row filter), usage without a transcript, and a session directory
/// holding neither sidecar. The tree is factored out because the pre-native
/// baseline below was captured by running this exact tree against the
/// pre-native scanner, and the capture is only honest if both runs build
/// byte-for-byte the same fixture.
fn seed_id_census(fixture: &GrokFixture) {
    fixture.add_row(
        "synthetic-session-011",
        1_780_000_000,
        "synthetic row-only summary",
    );
    fixture.add_row(
        "synthetic-session-012",
        1_780_000_000,
        "synthetic both-producers summary",
    );
    fixture.write_history(
        "synthetic-session-012",
        &format!("{NATIVE_TURN_1}\n{NATIVE_TURN_2}\n"),
    );
    fixture.write_usage("synthetic-session-012");
    fixture.write_history("synthetic-session-013", &format!("{NATIVE_TURN_1}\n"));
    fixture.write_usage("synthetic-session-013");
    fixture.add_row(
        "synthetic-session-014",
        1_780_000_000,
        "synthetic usage-sidecar summary",
    );
    fixture.write_usage("synthetic-session-014");
    fs::create_dir_all(
        fixture
            .sandbox
            .home()
            .join(".grok/sessions/synthetic-cwd/synthetic-session-015"),
    )
    .unwrap();
}

/// The ids the pre-native scanner produced for [`seed_id_census`]'s tree:
/// every `session_docs` row id and every `.usage` sidecar id, as literal
/// bytes. These literals were captured by running the census against the
/// tree with the native-collection change reverted (the scanner hunks of the
/// W982 commit) and passing an equality assertion against that reverted
/// scanner before being pinned here — they are the old scanner's own output,
/// not a reading of the current code.
fn pre_native_census() -> BTreeSet<&'static str> {
    [
        "grok.synthetic-machine.synthetic-session-011",
        "grok.synthetic-machine.synthetic-session-012",
        "grok.synthetic-machine.synthetic-session-012.usage",
        "grok.synthetic-machine.synthetic-session-014",
        "grok.synthetic-machine.synthetic-session-014.usage",
    ]
    .into_iter()
    .collect()
}

fn census_ids(scan: &scanner::ScanReport) -> BTreeSet<&str> {
    scan.records
        .iter()
        .map(|record| record.id.as_str())
        .collect()
}

/// GC-3: the scan's own JSON must state, per harness, the unit the archive
/// actually holds — DISTINCT staged session ids, not the record count and
/// not the store's row count. On [`seed_id_census`]'s tree the three counts
/// legitimately disagree (7 records over 6 ids over 3 rows) because a row
/// and its transcript are two records under one id while a usage sidecar
/// is one id of its own; the e2e gate compares `collected` against THIS
/// number, and `status --json` is where it reads it from. The last assertion
/// runs the real collector so the key is proven against the count of staged
/// session dirs, not against the scanner's own arithmetic.
#[test]
fn scan_report_json_states_staged_session_ids_not_records_or_rows() {
    let fixture = GrokFixture::new();
    seed_id_census(&fixture);

    let scan = fixture.scan();
    let value = scanner::scan_report_json(&scan);

    // Records, not ids: the row of session-012 and both-producers records
    // all together.
    assert_eq!(
        value["total_sessions"],
        json!({"kind": "known", "count": 7}),
        "total_sessions keeps counting SessionRecords (7 on this tree)"
    );
    let harnesses = value["harnesses"].as_array().unwrap();
    let probe = &harnesses
        .iter()
        .find(|p| p["harness"] == json!("grok"))
        .expect("the grok fixture harness is probed");
    assert_eq!(
        probe["session_count"],
        json!({"kind": "known", "count": 3}),
        "session_count keeps counting the store's session_docs rows"
    );
    assert_eq!(
        probe["found_staged"],
        json!({"kind": "known", "count": 6}),
        "found_staged counts DISTINCT staged ids: the 4 session ids \
         (011, 012, 013, 014) plus the .usage sidecar ids of row sessions \
         012 and 014"
    );

    // The promised unit is the collector's own: collect the tree and count
    // the session dirs the stage actually gains.
    let stage = fixture.collect_first(&scan);
    let machine_root = stage.join("sessions/synthetic-machine");
    let staged: Vec<_> = fs::read_dir(&machine_root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().unwrap().is_dir())
        .collect();
    assert_eq!(
        staged.len(),
        6,
        "collect stages exactly the distinct ids found_staged promised"
    );
}

/// Byte-identity of the id census: for one synthetic tree, the ids the
/// scanner produces must differ from the pre-native scanner's census by the
/// native-only session bodies alone. A row id or a `.usage` id that changed
/// shape — machine, separators, composition — fails here against the exact
/// bytes the old scanner emitted, and anything beyond native-only additions
/// (a new suffix for a row session's transcript, an id for a sidecar of a
/// row-less session) fails here as well.
#[test]
fn pre_native_ids_stay_byte_identical_and_native_only_ids_are_added() {
    let fixture = GrokFixture::new();
    seed_id_census(&fixture);

    let scan = fixture.scan();
    let ids = census_ids(&scan);

    // The only additions are the bodies of sessions the store has no row
    // for — the sessions that were invisible to the archive before.
    let additions: BTreeSet<&str> = ids.difference(&pre_native_census()).copied().collect();
    let native_only: BTreeSet<&str> = ["grok.synthetic-machine.synthetic-session-013"]
        .into_iter()
        .collect();
    assert_eq!(
        additions, native_only,
        "the transcript of a row session shares that row's id, and no sidecar \
         of a row-less session gains an id"
    );

    // The exact census: every pre-native id byte for byte, plus — and only
    // plus — the native-only session id.
    let mut expected = pre_native_census();
    expected.insert("grok.synthetic-machine.synthetic-session-013");
    assert_eq!(
        ids, expected,
        "the id census is the pre-native census plus native-only sessions"
    );

    // A session with both producers holds two records under ONE id: the row
    // export and the transcript file. The native collection adds a record,
    // never a second id.
    let both: Vec<_> = scan
        .records
        .iter()
        .filter(|record| record.id == "grok.synthetic-machine.synthetic-session-012")
        .collect();
    assert_eq!(both.len(), 2, "the row and its transcript share one id");
    assert_eq!(
        both.iter()
            .filter(|record| record.sqlite_layout.is_some())
            .count(),
        1,
        "one of the two records is the session_docs row export"
    );
    assert_eq!(
        both.iter()
            .filter(|record| record.sqlite_layout.is_none())
            .count(),
        1,
        "the other is the native transcript file record"
    );

    // The row-only session keeps exactly the one record it always had, and
    // the native-only session holds the transcript alone.
    assert_eq!(
        scan.records
            .iter()
            .filter(|record| record.id == "grok.synthetic-machine.synthetic-session-011")
            .count(),
        1,
        "a row-only session is unchanged"
    );
    assert_eq!(
        scan.records
            .iter()
            .filter(|record| record.id == "grok.synthetic-machine.synthetic-session-013")
            .count(),
        1,
        "a native-only session has the transcript as its only record"
    );
}
