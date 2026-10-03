//! DeepSeek Harness source wiring.
//!
//! The registry declares the format as `jsonl.zstd` and the session directory
//! rule as two levels below the root, because the layout measured on macOS
//! (app 0.2.0-rc.2, session format 4, 2026-10-03) is
//! `<DSH_HOME>/sessions/<slug-of-cwd>/<session-id>/session.v4.jsonl.zstd` and the
//! file name itself is a constant. These tests pin the two halves that the
//! scanner has to get right for that row: the native id comes from the session
//! directory (never from the file name, never from the lossy cwd slug), and a
//! `.jsonl.zstd` source is flagged compressed so the collector decodes it
//! instead of archiving zstd frames as text.

use chat_stasher::config::Config;
use chat_stasher::scanner::{self, current_platform};
use std::fs;

/// The bytes a real file starts with. The scanner is metadata-only and never
/// reads them; writing the frame magic keeps the fixture honest about the shape
/// without asking this test to decode anything.
const ZSTD_FRAME_MAGIC: [u8; 5] = [0x28, 0xB5, 0x2F, 0xFD, 0x00];

fn cell() -> serde_json::Value {
    serde_json::json!({
        "template": "~/.dsh/sessions/",
        "env_override": "DSH_HOME",
        "format": "jsonl.zstd",
        "confidence": "measured-locally",
        "session_dir": {"pattern": "*/*", "file": "session.v4.jsonl.zstd"}
    })
}

fn registry_for_this_platform() -> chat_stasher::scanner::HarnessRegistry {
    let paths = match current_platform() {
        "macos" => serde_json::json!({"macos": cell()}),
        "linux" => serde_json::json!({"linux": cell()}),
        "windows" => serde_json::json!({"windows": cell()}),
        other => panic!("unexpected platform {other}"),
    };
    serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "generated": "synthetic",
        "harnesses": [{
            "id": "deepseek-harness",
            "display_name": "DeepSeek Harness",
            "paths": paths
        }]
    }))
    .expect("the synthetic registry is well formed")
}

fn scan(root: &std::path::Path) -> chat_stasher::scanner::ScanReport {
    let config = Config {
        harness_roots: [(
            "deepseek-harness".to_string(),
            root.to_string_lossy().into_owned(),
        )]
        .into_iter()
        .collect(),
        ..Config::default()
    };
    scanner::scan_with_registry_and_machine(&config, &registry_for_this_platform(), "synthetic")
        .expect("the synthetic scan runs")
}

#[test]
fn a_dsh_session_is_named_by_its_directory_and_flagged_compressed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let transcript = root.join("--Users-me-code-project--/session-1f2e3d4c/session.v4.jsonl.zstd");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, ZSTD_FRAME_MAGIC).unwrap();

    let report = scan(&root);

    assert_eq!(
        report.records.len(),
        1,
        "one session directory holds one session"
    );
    assert_eq!(
        report.records[0].source.short(),
        "deepseek-harness",
        "the registry id maps to this build's source variant"
    );
    assert!(
        report.records[0].id.ends_with(".session-1f2e3d4c"),
        "the id is the session directory, not the constant file name: {}",
        report.records[0].id
    );
    assert!(
        !report.records[0].id.contains("Users-me-code-project"),
        "the cwd slug is lossy and must never become the id: {}",
        report.records[0].id
    );
    assert_eq!(report.records[0].absolute_path, transcript);
    assert!(
        report.records[0].compressed,
        "a .jsonl.zstd source is compressed; reading it as text would archive \
         zstd frames as if they were the conversation"
    );
}

/// A file that sits one level up is not a session. Without this the slug
/// directory could be mistaken for a session directory and its transcript
/// claimed twice, once under the slug and once under the real session.
#[test]
fn a_transcript_one_level_above_the_session_directory_is_not_a_session() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let misplaced = root.join("--Users-me-code-project--/session.v4.jsonl.zstd");
    fs::create_dir_all(misplaced.parent().unwrap()).unwrap();
    fs::write(&misplaced, ZSTD_FRAME_MAGIC).unwrap();

    let report = scan(&root);

    assert_eq!(
        report.records.len(),
        0,
        "the two-level rule accepts only <slug>/<session-id>/session.v4.jsonl.zstd"
    );
}

/// A patch-style release that stops writing compressed transcripts would leave
/// the row claiming `jsonl.zstd`; an uncompressed file of the same name must
/// not be claimed, so the format and the file agree or nothing is recorded.
#[test]
fn the_uncompressed_name_is_a_different_source_shape() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let plain = root.join("--Users-me-code-project--/session-1f2e3d4c/session.v4.jsonl");
    fs::create_dir_all(plain.parent().unwrap()).unwrap();
    fs::write(&plain, b"{}\n").unwrap();

    let report = scan(&root);

    assert_eq!(
        report.records.len(),
        0,
        "the declared file is session.v4.jsonl.zstd; a differently named file is not it"
    );
}
