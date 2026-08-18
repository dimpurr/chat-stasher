//! B95-RECHECK — counter-evidence for two "honest default" rationales that
//! did not survive re-inspection. Both tests fail against the pre-fix code.
//!
//! Only synthetic trees in temp dirs; assertions are counts and substrings.

use chat_stasher::collect;
use chat_stasher::collect::DestinationView;
use chat_stasher::config::Config;
use chat_stasher::doctor::GeminiRetention;
use chat_stasher::scanner::{self, HarnessRegistry};
use serde_json::json;
use std::fs;
use std::path::Path;

fn registry() -> HarnessRegistry {
    let cell = json!({
        "template": "~/.claude/projects",
        "format": "jsonl",
        "confidence": "源码确认",
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
            "id": "claude-code",
            "display_name": "synthetic",
            "paths": paths
        }]
    }))
    .unwrap()
}

fn dest<'a>() -> DestinationView<'a> {
    DestinationView::unreachable("fixture-destination")
}

fn scan(root: &Path) -> scanner::ScanReport {
    let config = Config {
        claude_projects_dir: Some(root.to_string_lossy().into_owned()),
        ..Config::default()
    };
    scanner::scan_with_registry(&config, &registry()).unwrap()
}

/// `collect.rs` claimed `prefix_bytes_validated` falls back to 0 only when
/// there is no reusable entry. The same expression is also reached when a
/// reusable entry EXISTS but its prefix hash check just FAILED — and there it
/// reported the recorded offset as "validated bytes" for a prefix that was
/// proven wrong. The full re-read that follows validates nothing.
#[test]
fn a_failed_prefix_hash_check_validates_zero_bytes_not_the_old_offset() {
    let dir = tempfile::TempDir::new().unwrap();
    let source_root = dir.path().join("source");
    fs::create_dir_all(&source_root).unwrap();
    let source = source_root.join("session.jsonl");
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");

    // Pass 1: 14 bytes committed, prefix hash recorded.
    fs::write(&source, b"one\ntwo\nthree\n").unwrap();
    let first = collect::collect_scan_report(
        &scan(&source_root),
        &stage,
        "fixture-machine",
        &state,
        20,
        &dest(),
    )
    .unwrap();
    assert_eq!(first.outcomes.len(), 1);

    // Pass 2: the file is NOT shorter (17 >= 14) so the reusable entry is
    // still selected, but its first 14 bytes were rewritten, so the recorded
    // prefix hash no longer matches.
    fs::write(&source, b"XXX\nYYY\nZZZ\nfour\n").unwrap();
    let second = collect::collect_scan_report(
        &scan(&source_root),
        &stage,
        "fixture-machine",
        &state,
        20,
        &dest(),
    )
    .unwrap();

    let outcome = &second.outcomes[0];
    assert!(
        outcome.reset,
        "a changed prefix must be reported as a reset"
    );
    assert_eq!(
        outcome.prefix_bytes_validated, 0,
        "the prefix hash check FAILED on this pass; no prefix byte was validated, \
         yet prefix_bytes_validated reported {} of them",
        outcome.prefix_bytes_validated
    );
    assert_eq!(
        second.prefix_bytes_validated, 0,
        "the run-level total inherits the same fabricated number"
    );
}

/// `doctor.rs` printed `parse_duration_days(max_age).unwrap_or(30.0)` — an
/// unparseable `maxAge` was rendered as the exact string "(30.0 days)", a
/// number nothing ever parsed, next to the user's own literal value.
#[test]
fn an_unparseable_max_age_is_not_rendered_as_thirty_days() {
    let policy = GeminiRetention {
        enabled: true,
        max_age: "3 months".to_string(),
        min_retention: "0d".to_string(),
        unreadable: None,
    };
    let summary = policy.summarize();
    assert!(
        !summary.contains("30.0 days"),
        "an unparsed maxAge must not be printed as a parsed duration: {summary}"
    );
    assert!(
        summary.contains("3 months"),
        "the user's literal value must still be shown: {summary}"
    );
}

/// A parseable-but-small maxAge keeps its real number.
#[test]
fn a_parseable_max_age_still_prints_its_real_number() {
    let policy = GeminiRetention {
        enabled: true,
        max_age: "7d".to_string(),
        min_retention: "0d".to_string(),
        unreadable: None,
    };
    let summary = policy.summarize();
    assert!(summary.contains("7.0 days"), "{summary}");
}
