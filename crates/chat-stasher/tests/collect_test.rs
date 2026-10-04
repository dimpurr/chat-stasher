//! Incremental collector tests use only synthetic source trees in temp dirs.
//! Assertions inspect bytes/counts, while test output stays metadata-only.

use chat_stasher::activity;
use chat_stasher::collect;
use chat_stasher::collect::DestinationView;
use chat_stasher::config::Config;
use chat_stasher::fts;
use chat_stasher::provenance::{self, SessionProvenance};
use chat_stasher::scanner::{self, HarnessRegistry};
use chat_stasher::selector::{Selector, SessionMeta, TimeBounds, Verdict};
use chat_stasher::store;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

static HOME_LOCK: Mutex<()> = Mutex::new(());

fn registry() -> HarnessRegistry {
    let cell = json!({
        "template": "~/.claude/projects",
        "format": "jsonl",
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
            "id": "claude-code",
            "display_name": "synthetic",
            "paths": paths
        }]
    }))
    .unwrap()
}

/// These tests only exercise the local read path, so the destination archive
/// is deliberately not reachable: a debt still owed on the stage must be
/// provable without ever asking it.
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

#[test]
fn reads_new_bytes_then_resets_on_truncate_and_rewrite() {
    let dir = tempfile::TempDir::new().unwrap();
    let source_root = dir.path().join("source");
    fs::create_dir_all(&source_root).unwrap();
    let source = source_root.join("session.jsonl");
    fs::write(&source, b"one\ntwo\nthree\n").unwrap();
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");

    let first = collect::collect_scan_report(
        &scan(&source_root),
        &stage,
        "fixture-machine",
        &state,
        20,
        &dest(),
    )
    .unwrap();
    let id = scan(&source_root).records[0].id.clone();
    fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(b"four\nfive\n")
        .unwrap();
    let second = collect::collect_scan_report(
        &scan(&source_root),
        &stage,
        "fixture-machine",
        &state,
        20,
        &dest(),
    )
    .unwrap();
    let after_append = store::concat_shards(&stage, "fixture-machine", &id).unwrap();
    assert_eq!(after_append, b"one\ntwo\nthree\nfour\nfive\n");
    assert_eq!(first.lines_written, 3);
    assert_eq!(second.lines_written, 2);
    assert_eq!(second.delta_bytes_read, b"four\nfive\n".len() as u64);
    assert_eq!(second.reset_records, 0);

    // Same byte length as the committed source, but a changed committed
    // prefix. The SHA-256 guard must still force a full reread.
    let same_length_rewrite = b"aaaa\nbbbb\ncccc\ndddd\neee\n";
    assert_eq!(
        same_length_rewrite.len(),
        second.outcomes[0].source_bytes as usize
    );
    fs::write(&source, same_length_rewrite).unwrap();
    let third = collect::collect_scan_report(
        &scan(&source_root),
        &stage,
        "fixture-machine",
        &state,
        20,
        &dest(),
    )
    .unwrap();
    let after_reset = store::concat_shards(&stage, "fixture-machine", &id).unwrap();
    assert_eq!(third.reset_records, 1);
    assert_eq!(third.lines_written, 5);
    assert_eq!(third.delta_bytes_read, same_length_rewrite.len() as u64);
    assert_eq!(
        after_reset.len(),
        after_append.len() + same_length_rewrite.len()
    );

    // A shorter rewrite takes the other reset branch.
    let shorter_rewrite = b"short\n";
    fs::write(&source, shorter_rewrite).unwrap();
    let fourth = collect::collect_scan_report(
        &scan(&source_root),
        &stage,
        "fixture-machine",
        &state,
        20,
        &dest(),
    )
    .unwrap();
    let after_truncate = store::concat_shards(&stage, "fixture-machine", &id).unwrap();
    assert_eq!(fourth.reset_records, 1);
    assert_eq!(fourth.lines_written, 1);
    assert_eq!(fourth.delta_bytes_read, shorter_rewrite.len() as u64);
    assert_eq!(
        after_truncate.len(),
        after_reset.len() + shorter_rewrite.len()
    );
    println!(
        "incremental first_lines={} append_lines={} append_delta_bytes={} same_length_rewrite_bytes={} same_length_reset={} truncate_rewrite_bytes={} truncate_reset={} stage_bytes={}",
        first.lines_written,
        second.lines_written,
        second.delta_bytes_read,
        third.delta_bytes_read,
        third.reset_records,
        fourth.delta_bytes_read,
        fourth.reset_records,
        after_truncate.len()
    );
}

#[test]
fn missing_stage_shard_reconciles_state_and_rereads() {
    let dir = tempfile::TempDir::new().unwrap();
    let source_root = dir.path().join("source");
    fs::create_dir_all(&source_root).unwrap();
    let source = source_root.join("session.jsonl");
    fs::write(&source, b"one\ntwo\n").unwrap();
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");
    let scan_report = scan(&source_root);
    let id = scan_report.records[0].id.clone();

    let first =
        collect::collect_scan_report(&scan_report, &stage, "fixture-machine", &state, 20, &dest())
            .unwrap();
    assert_eq!(first.lines_written, 2);
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &id).unwrap(),
        b"one\ntwo\n"
    );

    fs::remove_dir_all(&stage).unwrap();
    let second = collect::collect_scan_report(
        &scan(&source_root),
        &stage,
        "fixture-machine",
        &state,
        20,
        &dest(),
    )
    .unwrap();
    let restored = store::concat_shards(&stage, "fixture-machine", &id).unwrap();
    assert_eq!(second.reset_records, 1);
    assert_eq!(second.unchanged_records, 0);
    assert_eq!(second.lines_written, 2);
    assert_eq!(restored, b"one\ntwo\n");
}

#[test]
fn incomplete_tail_is_left_for_the_next_read() {
    let dir = tempfile::TempDir::new().unwrap();
    let source_root = dir.path().join("source");
    fs::create_dir_all(&source_root).unwrap();
    let source = source_root.join("session.jsonl");
    fs::write(&source, b"complete\npartial").unwrap();
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");
    let first_scan = scan(&source_root);
    let id = first_scan.records[0].id.clone();

    let first =
        collect::collect_scan_report(&first_scan, &stage, "fixture-machine", &state, 20, &dest())
            .unwrap();
    let first_stage = store::concat_shards(&stage, "fixture-machine", &id).unwrap();
    assert_eq!(first.lines_written, 1);
    assert_eq!(first_stage, b"complete\n");

    fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    let second = collect::collect_scan_report(
        &scan(&source_root),
        &stage,
        "fixture-machine",
        &state,
        20,
        &dest(),
    )
    .unwrap();
    let second_stage = store::concat_shards(&stage, "fixture-machine", &id).unwrap();
    assert_eq!(second.lines_written, 1);
    // The cursor stays at the last complete newline, so the old partial tail
    // is reread together with the newly appended newline. It is not staged
    // twice; rereading it is the deliberate multi-read safety cost.
    assert_eq!(second.delta_bytes_read, b"partial\n".len() as u64);
    assert_eq!(second_stage, b"complete\npartial\n");
    println!(
        "half_line first_written={} first_stage_bytes={} second_written={} second_delta_bytes={} final_stage_bytes={}",
        first.lines_written,
        first_stage.len(),
        second.lines_written,
        second.delta_bytes_read,
        second_stage.len()
    );
}

use std::io::Write;

#[test]
fn antigravity_multi_root_resume_collects_only_the_new_suffix_and_restores_both_surfaces() {
    let _guard = HOME_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let old_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &home);
    let home_reset = HomeReset(old_home);

    let user_line = b"{\"created_at\":\"2026-10-01T10:00:00Z\",\"type\":\"USER_INPUT\",\"content\":\"synthetic prompt\"}\n";
    let assistant_line = b"{\"created_at\":\"2026-10-01T10:00:01Z\",\"type\":\"PLANNER_RESPONSE\",\"content\":\"synthetic reply\"}\n";
    let mut roots = Vec::new();
    for (root, surface, content) in [
        ("antigravity-cli", "cli", user_line.to_vec()),
        (
            "antigravity-ide",
            "ide",
            [user_line.as_slice(), assistant_line.as_slice()].concat(),
        ),
        ("antigravity", "app", user_line.to_vec()),
    ] {
        let source = home
            .join(".gemini")
            .join(root)
            .join("brain/session-same-uuid/.system_generated/logs/transcript.jsonl");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, content).unwrap();
        let cell = json!({
            "template": format!("~/.gemini/{root}/brain/"),
            "format": "jsonl",
            "confidence": "source-confirmed",
            "source": "synthetic fixture",
            "session_dir": { "pattern": "*", "file": ".system_generated/logs/transcript.jsonl" }
        });
        let paths = match scanner::current_platform() {
            "macos" => json!({"macos": cell}),
            "linux" => json!({"linux": cell}),
            "windows" => json!({"windows": cell}),
            platform => panic!("unexpected platform: {platform}"),
        };
        roots.push(json!({
            "id": root,
            "paths": paths,
            "provenance": { "surface": [surface] }
        }));
    }
    let registry: HarnessRegistry = serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic fixture",
        "harnesses": [{ "id": "google-antigravity", "display_name": "fixture", "source_roots": roots }]
    })).unwrap();
    let scan =
        scanner::scan_with_registry_and_machine(&Config::default(), &registry, "fixture-machine")
            .unwrap();
    let session_id = "google-antigravity.fixture-machine.session-same-uuid";
    assert_eq!(
        scan.records
            .iter()
            .filter(|record| record.id == session_id)
            .count(),
        3
    );

    let stage = temp.path().join("stage");
    let state = temp.path().join("state");
    let first = collect::collect_scan_report(&scan, &stage, "fixture-machine", &state, 20, &dest())
        .unwrap();
    assert_eq!(
        first.lines_written, 2,
        "the resumed snapshot contributes only its suffix; the third identical stream adds nothing"
    );
    let body = store::concat_shards(&stage, "fixture-machine", session_id).unwrap();
    assert_eq!(
        body,
        [user_line.as_slice(), assistant_line.as_slice()].concat()
    );

    let body_text = std::str::from_utf8(&body).unwrap();
    let normalized = chat_stasher::normalize::normalize("google-antigravity", body_text);
    assert_eq!(normalized.message_total, 2);
    let indexed = fts::extract_index_document_for("google-antigravity", &body).unwrap();
    assert!(indexed.not_indexable.is_none());
    assert!(indexed.body.contains("synthetic prompt"));
    assert!(indexed.body.contains("synthetic reply"));
    let lines: Vec<_> = body_text.lines().collect();
    let times = activity::analyze_session("google-antigravity", &lines);
    assert_eq!(times.line_count, 2);
    assert!(matches!(times.time_source, activity::TimeSource::Exact));

    let observations = provenance::read_observations(&stage, "fixture-machine").unwrap();
    assert_eq!(
        observations.len(),
        3,
        "each source root records its immutable observation"
    );
    let mut dimensions = SessionProvenance::default();
    provenance::merge_verified_observations(
        &mut dimensions,
        session_id,
        &observations,
        &[user_line.to_vec(), assistant_line.to_vec()],
    );
    assert_eq!(dimensions.surface, ["app", "cli", "ide"]);
    let cli = Selector {
        surface: Some("cli".into()),
        ..Default::default()
    };
    let app = Selector {
        surface: Some("app".into()),
        ..Default::default()
    };
    let meta = SessionMeta {
        machine: "fixture-machine",
        session_id,
        harness: Some("google-antigravity"),
        surfaces: Some(&dimensions.surface),
        first_unix: times.first_unix,
        last_unix: times.last_unix,
        time_bounds: TimeBounds::Complete,
        time_why: None,
    };
    assert_eq!(cli.select(&meta), Verdict::Selected);
    assert_eq!(app.select(&meta), Verdict::Selected);
    drop(home_reset);
}

#[test]
fn new_session_does_not_probe_destination_for_an_archive_prefix() {
    let temp = tempfile::TempDir::new().unwrap();
    let source_root = temp.path().join("source");
    fs::create_dir_all(&source_root).unwrap();
    fs::write(
        source_root.join("session.jsonl"),
        b"{\"type\":\"USER_INPUT\"}\n",
    )
    .unwrap();
    let scan_report = scan(&source_root);
    let destination = DestinationView::new("fresh-session", |_| {
        panic!("a genuinely new session must not open the archive")
    });
    let stage = temp.path().join("stage");
    let report = collect::collect_scan_report(
        &scan_report,
        &stage,
        "fixture-machine",
        &temp.path().join("state"),
        20,
        &destination,
    )
    .unwrap();
    assert!(report.errors.is_empty());
}

struct HomeReset(Option<std::ffi::OsString>);

impl Drop for HomeReset {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }
}
