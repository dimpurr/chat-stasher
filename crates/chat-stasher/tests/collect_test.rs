//! Incremental collector tests use only synthetic source trees in temp dirs.
//! Assertions inspect bytes/counts, while test output stays metadata-only.

use chat_stasher::collect;
use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry};
use chat_stasher::store;
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

    let first =
        collect::collect_scan_report(&scan(&source_root), &stage, "fixture-machine", &state, 20)
            .unwrap();
    let id = scan(&source_root).records[0].id.clone();
    fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(b"four\nfive\n")
        .unwrap();
    let second =
        collect::collect_scan_report(&scan(&source_root), &stage, "fixture-machine", &state, 20)
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
    let third =
        collect::collect_scan_report(&scan(&source_root), &stage, "fixture-machine", &state, 20)
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
    let fourth =
        collect::collect_scan_report(&scan(&source_root), &stage, "fixture-machine", &state, 20)
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
        collect::collect_scan_report(&scan_report, &stage, "fixture-machine", &state, 20).unwrap();
    assert_eq!(first.lines_written, 2);
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &id).unwrap(),
        b"one\ntwo\n"
    );

    fs::remove_dir_all(&stage).unwrap();
    let second =
        collect::collect_scan_report(&scan(&source_root), &stage, "fixture-machine", &state, 20)
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
        collect::collect_scan_report(&first_scan, &stage, "fixture-machine", &state, 20).unwrap();
    let first_stage = store::concat_shards(&stage, "fixture-machine", &id).unwrap();
    assert_eq!(first.lines_written, 1);
    assert_eq!(first_stage, b"complete\n");

    fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    let second =
        collect::collect_scan_report(&scan(&source_root), &stage, "fixture-machine", &state, 20)
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
