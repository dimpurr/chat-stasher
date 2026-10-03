//! Grok CLI usage sidecars are archived as raw files under their session identity.

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::{scanner, store};
use serde_json::json;
use std::fs;

#[path = "../src/test_support.rs"]
mod test_support;

#[test]
fn usage_json_is_archived_byte_for_byte_under_a_linked_session_id() {
    let sandbox = test_support::Sandbox::new();
    let sessions = sandbox.home().join(".grok/sessions");
    let db = sessions.join("session_search.sqlite");
    fs::create_dir_all(&sessions).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL, updated_at INTEGER NOT NULL, title TEXT NOT NULL, content TEXT NOT NULL, content_hash TEXT NOT NULL);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_docs VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            "synthetic-session-002",
            "/synthetic/work",
            1_780_000_000_i64,
            "synthetic title",
            "synthetic body",
            "synthetic hash"
        ],
    )
    .unwrap();
    drop(conn);

    let usage = sessions
        .join("synthetic-cwd")
        .join("synthetic-session-002")
        .join("usage.json");
    fs::create_dir_all(usage.parent().unwrap()).unwrap();
    let raw = br#"{"modelUsage":{"grok-test":{"inputTokens":12,"cachedReadTokens":7,"outputTokens":3,"totalTokens":15,"otherCounter":2},"grok-test-2":{"inputTokens":5,"cachedReadTokens":1,"outputTokens":4,"totalTokens":9}}}"#;
    fs::write(&usage, raw).unwrap();

    let config = Config {
        harness_roots: [("grok".to_string(), db.to_string_lossy().into())]
            .into_iter()
            .collect(),
        ..Config::default()
    };
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
    let registry: scanner::HarnessRegistry = serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic fixture",
        "harnesses": [{"id": "grok", "display_name": "Grok CLI fixture", "paths": paths}]
    }))
    .unwrap();
    let machine = "synthetic-machine";
    let report = scanner::scan_with_registry_and_machine(&config, &registry, machine).unwrap();
    let sqlite_id = format!("grok.{machine}.synthetic-session-002");
    let usage_id = format!("{sqlite_id}.usage");
    assert!(report.records.iter().any(|record| record.id == sqlite_id));
    assert!(report.records.iter().any(|record| record.id == usage_id));

    let stage = sandbox.root().join("stage");
    let state = sandbox.root().join("state/collector.json");
    collect::collect_scan_report(
        &report,
        &stage,
        &machine,
        &state,
        20,
        &DestinationView::unreachable("synthetic-destination"),
    )
    .unwrap();

    let archived = store::concat_shards(&stage, &machine, &usage_id).unwrap();
    assert_eq!(archived.strip_suffix(b"\n"), Some(raw.as_slice()));
}
