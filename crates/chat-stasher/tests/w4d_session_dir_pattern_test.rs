use chat_stasher::config::Config;
use chat_stasher::scanner::{self, current_platform};
use std::fs;

#[test]
fn root_relative_two_level_session_directory_pattern_is_scanned() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let transcript = root.join("workspace/session-uuid/transcript.jsonl");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, b"{}\n").unwrap();
    // A generic exact-depth `*/*` rule must keep its original session owner
    // through implementation paths such as agents/main/wire.jsonl. A nested
    // directory that resembles another session must not become a new owner.
    let implementation_transcript =
        root.join("workspace/session-uuid/agents/main/session-false/transcript.jsonl");
    fs::create_dir_all(implementation_transcript.parent().unwrap()).unwrap();
    fs::write(&implementation_transcript, b"{}\n").unwrap();

    let cell = serde_json::json!({
        "template": "~/synthetic-sessions",
        "format": "jsonl",
        "confidence": "measured-locally",
        "session_dir": {"pattern": "*/*", "file": "transcript.jsonl"}
    });
    let paths = match current_platform() {
        "macos" => serde_json::json!({"macos": cell}),
        "linux" => serde_json::json!({"linux": cell}),
        "windows" => serde_json::json!({"windows": cell}),
        other => panic!("unexpected platform {other}"),
    };
    let registry = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "generated": "synthetic",
        "harnesses": [{"id": "claude-code", "display_name": "fixture", "paths": paths}]
    }))
    .unwrap();
    let config = Config {
        harness_roots: [(
            "claude-code".to_string(),
            root.to_string_lossy().into_owned(),
        )]
        .into_iter()
        .collect(),
        ..Config::default()
    };
    let report = scanner::scan_with_registry_and_machine(&config, &registry, "synthetic").unwrap();

    assert_eq!(report.records.len(), 1);
    assert!(report.records[0].id.ends_with(".session-uuid"));
    assert_eq!(report.records[0].absolute_path, transcript);
}
