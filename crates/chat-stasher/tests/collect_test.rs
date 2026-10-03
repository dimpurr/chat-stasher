//! Incremental collector tests use only synthetic source trees in temp dirs.
//! Assertions inspect bytes/counts, while test output stays metadata-only.

use chat_stasher::collect;
use chat_stasher::collect::DestinationView;
use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry};
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

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
fn collect_records_subagent_provenance_and_preserves_raw_source_bytes() {
    let sandbox = test_support::Sandbox::new();
    let source_root = sandbox.root().join(".claude/projects");
    let source = source_root.join("project-fixture/parent-fixture/subagents/agent-fixture.jsonl");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    let raw = br#"{"type":"user","message":{"content":"synthetic fixture"}}"#;
    let raw = [raw.as_slice(), b"\n"].concat();
    fs::write(&source, &raw).unwrap();
    let stage = sandbox.root().join("stage");
    let state = sandbox.root().join("state");

    let scan_report = scan(&source_root);
    assert_eq!(scan_report.records.len(), 1);
    let session_id = scan_report.records[0].id.clone();
    collect::collect_scan_report(&scan_report, &stage, "fixture-machine", &state, 20, &dest())
        .unwrap();

    assert_eq!(fs::read(&source).unwrap(), raw);
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &session_id).unwrap(),
        raw
    );

    let mut rebuild = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    rebuild.args([
        "activity-index",
        "--stage",
        stage.to_str().unwrap(),
        "--machine",
        "fixture-machine",
        "--rebuild",
    ]);
    sandbox.apply(&mut rebuild);
    let output = rebuild.output().unwrap();
    assert!(output.status.success(), "activity-index rebuild failed");

    let cfg = StoreConfig {
        repo_root: sandbox
            .root()
            .join("archive")
            .to_string_lossy()
            .into_owned(),
        key_file: sandbox.root().join("archive-key.json"),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(test_support::rustic_cache_root(sandbox.root())),
        no_cache: false,
    };
    let key = MasterKey::new();
    store::persist_key_file(&cfg, &key).unwrap();
    let archive = BackupStore::new(cfg, "fixture-machine".to_string());
    assert!(archive.push(&stage, &key).unwrap().files_new > 0);

    let (restored, _) = archive
        .read_session_concat("fixture-machine", &session_id, &key)
        .unwrap();
    assert_eq!(
        restored, raw,
        "archive readback preserves the raw source bytes"
    );
    let (repo, _) = archive.open_indexed(&key).unwrap();
    let snapshot = repo
        .get_all_snapshots()
        .unwrap()
        .into_iter()
        .filter(|snapshot| snapshot.hostname == "fixture-machine")
        .max()
        .unwrap();
    let stage_relative = stage.canonicalize().unwrap();
    let stage_relative = stage_relative.strip_prefix("/").unwrap_or(&stage_relative);
    let index_path = stage_relative.join("meta/fixture-machine/activity-v1.jsonl");
    let node = repo
        .node_from_snapshot_and_path(&snapshot, &index_path.to_string_lossy())
        .unwrap();
    let mut archived_index = Vec::new();
    repo.dump(&node, &mut archived_index).unwrap();
    let row: serde_json::Value = std::str::from_utf8(&archived_index)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .find(|row: &serde_json::Value| row["session_id"] == session_id)
        .unwrap();
    assert_eq!(row["session_provenance"]["source_path_class"], "subagents");
    assert_eq!(
        row["session_provenance"]["parent_session_ref"],
        "parent-fixture"
    );
    assert_eq!(row["source_path_class"], "subagents/");
    assert!(!String::from_utf8_lossy(&archived_index)
        .contains(sandbox.root().to_string_lossy().as_ref()));
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
