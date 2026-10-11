//! Grok CLI native transcripts (`chat_history.jsonl`) are archived into the
//! session's own body, next to the `session_docs` export.
//!
//! The reader (GC-1) makes one body of both observations: a body whose native
//! records render at least one message reads as one conversation — the native
//! turns — and must not append the legacy plain-text summary again, while the
//! raw body keeps both. These tests pin the collector's half of that contract:
//! every `chat_history.jsonl` reaches the archive, including sessions the
//! `session_docs` store has no row for, each file keeps an ordinary
//! append-only cursor, and no existing archived id changes.
//!
//! All fixtures are synthetic; no real conversation text, ids or names.

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::normalize::{normalize, Block};
use chat_stasher::{scanner, store};
use serde_json::json;
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

    fn collect_again(&self, stage: &std::path::Path, scan: &scanner::ScanReport) {
        let state = self.sandbox.root().join("state/collector.json");
        collect::collect_scan_report(
            scan,
            stage,
            "synthetic-machine",
            &state,
            20,
            &DestinationView::unreachable("synthetic-destination"),
        )
        .unwrap();
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
