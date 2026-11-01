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
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

#[path = "../src/test_support.rs"]
mod test_support;

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

#[test]
fn antigravity_fresh_roots_preserve_ambiguous_overlap_and_all_surfaces() {
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
    // Each root's exact input must survive, including equal bodies. There is
    // no verified chronological order between independent source roots.
    let mut expected_shards: Vec<Vec<u8>> = scan
        .records
        .iter()
        .map(|record| fs::read(&record.absolute_path).unwrap())
        .collect();
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
        first.lines_written, 4,
        "independent roots have no verified event identity; byte overlap cannot authorize dropping a repeated turn"
    );
    let body = store::concat_shards(&stage, "fixture-machine", session_id).unwrap();
    let mut shard_entries = store::sealed_shard_entries(&store::session_shard_dir(
        &stage,
        "fixture-machine",
        session_id,
    ))
    .unwrap();
    shard_entries.sort_by_key(|(sequence, _)| *sequence);
    let captured_shards: Vec<Vec<u8>> = shard_entries
        .iter()
        .map(|(_, path)| fs::read(path).unwrap())
        .collect();
    let mut captured_multiset = captured_shards.clone();
    captured_multiset.sort();
    expected_shards.sort();
    assert_eq!(
        captured_multiset, expected_shards,
        "each complete source body survives with its multiplicity"
    );
    assert_eq!(body, captured_shards.concat());

    let body_text = std::str::from_utf8(&body).unwrap();
    let normalized = chat_stasher::normalize::normalize("google-antigravity", body_text);
    assert_eq!(normalized.message_total, 4);
    let indexed = fts::extract_index_document_for("google-antigravity", &body).unwrap();
    assert!(indexed.not_indexable.is_none());
    assert!(indexed.body.contains("synthetic prompt"));
    assert!(indexed.body.contains("synthetic reply"));
    let lines: Vec<_> = body_text.lines().collect();
    let times = activity::analyze_session("google-antigravity", &lines);
    assert_eq!(times.line_count, 4);
    assert!(matches!(times.time_source, activity::TimeSource::Exact));

    let observations = provenance::read_observations(&stage, "fixture-machine").unwrap();
    assert_eq!(
        observations.len(),
        3,
        "each source root records its immutable observation"
    );
    let mut dimensions = SessionProvenance::default();
    let sequence_bodies: Vec<_> = shard_entries
        .iter()
        .zip(&captured_shards)
        .map(|((sequence, _), body)| (Some(*sequence), body.clone()))
        .collect();
    provenance::merge_verified_shard_sequence_observations(
        &mut dimensions,
        session_id,
        &observations,
        &sequence_bodies,
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
    let unchanged =
        collect::collect_scan_report(&scan, &stage, "fixture-machine", &state, 20, &dest())
            .unwrap();
    assert!(unchanged.errors.is_empty());
    assert_eq!(
        unchanged.lines_written, 0,
        "verified source cursors remain incremental"
    );
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", session_id).unwrap(),
        body
    );
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

// ---------------------------------------------------------------------------
// GB-2: claude-code workflow journals
// ---------------------------------------------------------------------------

/// Scan a claude-code projects tree as the fixed `fixture-machine`, so the
/// scanned record ids embed the same machine name collect stages under.
fn scan_journals(root: &Path) -> scanner::ScanReport {
    let config = Config {
        claude_projects_dir: Some(root.to_string_lossy().into_owned()),
        ..Config::default()
    };
    scanner::scan_with_registry_and_machine(&config, &registry(), "fixture-machine").unwrap()
}

/// The sealed shard bodies of one session slot, in global sequence order.
fn sealed_shard_bodies(stage: &Path, machine: &str, session_id: &str) -> Vec<Vec<u8>> {
    let mut entries =
        store::sealed_shard_entries(&store::session_shard_dir(stage, machine, session_id)).unwrap();
    entries.sort_by_key(|(sequence, _)| *sequence);
    entries
        .iter()
        .map(|(_, path)| fs::read(path).unwrap())
        .collect()
}

/// The session slots the stage holds for one machine partition, so a test
/// can prove a record's bytes went nowhere else.
fn staged_session_slots(stage: &Path, machine: &str) -> std::collections::BTreeSet<String> {
    let machine_root = store::session_shard_dir(stage, machine, "-");
    let machine_dir = machine_root.parent().unwrap();
    fs::read_dir(machine_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn run_id(run: &str) -> String {
    format!("claude-code.fixture-machine.{run}")
}

/// GB-2 regression. Two workflow journals below different parent sessions
/// are two sessions, not one: each stages under its own workflow-run id, and
/// neither slot ever receives the other run's bytes. Before the structural
/// id, both journals shared the constant `journal` filename stem, so collect
/// sealed both into a single merged slot under one session id.
#[test]
fn workflow_journals_stage_as_separate_sessions() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("projects");
    let run_a = "wf_019d7a00-0000-7000-8000-00000000000a";
    let run_b = "wf_019d7b00-0000-7000-8000-00000000000b";
    let journal_a = root
        .join("-fixture-proj-a/019d7a01-0000-7000-8000-000000000001")
        .join("subagents/workflows")
        .join(run_a)
        .join("journal.jsonl");
    let journal_b = root
        .join("-fixture-proj-b/019d7b02-0000-7000-8000-000000000002")
        .join("subagents/workflows")
        .join(run_b)
        .join("journal.jsonl");
    for (journal, body) in [
        (&journal_a, &b"{\"synthetic\":\"a\"}\n"[..]),
        (&journal_b, &b"{\"synthetic\":\"b\"}\n"[..]),
    ] {
        fs::create_dir_all(journal.parent().unwrap()).unwrap();
        fs::write(journal, body).unwrap();
    }
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");

    let scan_report = scan_journals(&root);
    assert_eq!(
        scan_report.records.len(),
        2,
        "one record per workflow journal"
    );
    let id_a = format!("claude-code.fixture-machine.{run_a}");
    let id_b = format!("claude-code.fixture-machine.{run_b}");
    let scanned_ids: std::collections::BTreeSet<_> = scan_report
        .records
        .iter()
        .map(|record| record.id.as_str())
        .collect();
    assert_eq!(
        scanned_ids,
        [id_a.as_str(), id_b.as_str()].into_iter().collect(),
        "each journal ids by its workflow-run directory"
    );

    let collected =
        collect::collect_scan_report(&scan_report, &stage, "fixture-machine", &state, 20, &dest())
            .unwrap();
    assert_eq!(
        collected.changed_records, 2,
        "both journals delivered content in this pass"
    );
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &id_a).unwrap(),
        b"{\"synthetic\":\"a\"}\n",
        "run a's slot holds only run a's journal"
    );
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &id_b).unwrap(),
        b"{\"synthetic\":\"b\"}\n",
        "run b's slot holds only run b's journal"
    );
}

/// GB-2 handover. A pre-W978 stage holds a journal under the shared stem id
/// `claude-code.<machine>.journal` (on m3 every workflow run had collapsed
/// into that one slot). The id rule changes; what happens to the data already
/// staged under the old id must be the append-only answer: the old slot is
/// never rewritten, and the per-path cursor hands only the *new* bytes to the
/// journal's own workflow-run id. The pass below rewrites the scanned ids to
/// the shared shape collect used to produce — no real stage is involved, only
/// this synthetic tree — which is why the first slot lands under a `~`-less
/// stem id with the exact bytes that precede the id change.
#[test]
fn journal_id_change_hands_only_new_bytes_to_the_workflow_run_id() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("projects");
    let run_a = "wf_019d7c00-0000-7000-8000-00000000000a";
    let journal = root
        .join("-fixture-proj/019d7c01-0000-7000-8000-000000000001")
        .join("subagents/workflows")
        .join(run_a)
        .join("journal.jsonl");
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    let first_line = &b"{\"synthetic\":\"1\"}\n"[..];
    fs::write(&journal, first_line).unwrap();
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");

    // The pass a pre-W978 build would have run: the record id is the
    // filename stem, shared by every workflow journal on the machine.
    let shared_id = "claude-code.fixture-machine.journal".to_string();
    let mut pre = scan_journals(&root);
    assert_eq!(pre.records.len(), 1);
    for record in &mut pre.records {
        record.id = shared_id.clone();
    }
    collect::collect_scan_report(&pre, &stage, "fixture-machine", &state, 20, &dest()).unwrap();
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &shared_id).unwrap(),
        first_line,
        "the pre-change pass staged the whole journal under the shared id"
    );

    // The journal gains one more synthetic event; the scan now ids it by its
    // workflow run.
    let second_line = &b"{\"synthetic\":\"2\"}\n"[..];
    fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(second_line)
        .unwrap();
    let post = scan_journals(&root);
    let run_id = format!("claude-code.fixture-machine.{run_a}");
    assert_eq!(
        post.records[0].id, run_id,
        "the journal now ids by its workflow-run directory"
    );
    let second =
        collect::collect_scan_report(&post, &stage, "fixture-machine", &state, 20, &dest())
            .unwrap();
    assert_eq!(
        second.reset_records, 0,
        "the per-path cursor survives the id change: no full reread"
    );
    assert_eq!(second.delta_bytes_read, second_line.len() as u64);
    assert_eq!(second.changed_records, 1);
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &shared_id).unwrap(),
        first_line,
        "the old shared slot is frozen exactly as the pre-change pass sealed it"
    );
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &run_id).unwrap(),
        second_line,
        "only the bytes after the id change land under the workflow-run id"
    );
}

/// GB-2 handover, many-runs shape (W980). The single-journal test above pins
/// the id change for one run; the machine the diagnosis measured had many
/// journals — nine runs, three here — whose shared `journal` stem had fused
/// every run's pre-upgrade bytes into one merged slot, one sealed shard per
/// journal and one cursor per journal path. The first post-W978 pass must
/// hand that whole shape over without rewriting or losing anything: the
/// merged slot stays frozen exactly as the old build sealed it, each
/// per-path cursor still proves itself against that shared slot, and only
/// the bytes appended after the upgrade land under each run's own
/// workflow-run id. A pass that changed nothing adds nothing anywhere.
#[test]
fn journal_id_change_hands_over_many_runs_from_one_merged_slot() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("projects");
    // One journal per run, one run per project directory; the project
    // directories sort a, b, c, which fixes the order collect seals the
    // per-path shards in.
    let runs = [
        (
            "wf_019d7f00-0000-7000-8000-00000000000a",
            "-fixture-proj-a",
            "019d7f01-0000-7000-8000-000000000001",
            "{\"synthetic\":\"pre-a\"}\n",
            "{\"synthetic\":\"post-a\"}\n",
        ),
        (
            "wf_019d7f00-0000-7000-8000-00000000000b",
            "-fixture-proj-b",
            "019d7f02-0000-7000-8000-000000000002",
            "{\"synthetic\":\"pre-b\"}\n",
            "{\"synthetic\":\"post-b\"}\n",
        ),
        (
            "wf_019d7f00-0000-7000-8000-00000000000c",
            "-fixture-proj-c",
            "019d7f03-0000-7000-8000-000000000003",
            "{\"synthetic\":\"pre-c\"}\n",
            "{\"synthetic\":\"post-c\"}\n",
        ),
    ];
    let mut journals = Vec::new();
    for (run, project, parent, pre_body, post_body) in &runs {
        let journal = root
            .join(project)
            .join(parent)
            .join("subagents/workflows")
            .join(run)
            .join("journal.jsonl");
        fs::create_dir_all(journal.parent().unwrap()).unwrap();
        fs::write(&journal, pre_body.as_bytes()).unwrap();
        journals.push((run, journal, pre_body, post_body));
    }
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");

    // The pass a pre-W978 build ran: every journal carries the shared stem
    // id, so every journal's whole body seals into the one merged slot,
    // behind one cursor per journal path.
    let shared_id = "claude-code.fixture-machine.journal".to_string();
    let mut pre = scan_journals(&root);
    assert_eq!(pre.records.len(), 3, "one record per workflow journal");
    for record in &mut pre.records {
        record.id = shared_id.clone();
    }
    let first =
        collect::collect_scan_report(&pre, &stage, "fixture-machine", &state, 20, &dest()).unwrap();
    assert_eq!(
        first.changed_records, 3,
        "the old shared slot received every journal's bytes"
    );
    let expected_pre_bodies: Vec<Vec<u8>> = journals
        .iter()
        .map(|(_, _, pre_body, _)| pre_body.as_bytes().to_vec())
        .collect();
    assert_eq!(
        sealed_shard_bodies(&stage, "fixture-machine", &shared_id),
        expected_pre_bodies,
        "the merged slot holds one sealed shard per journal, no loss and no fusion"
    );
    let frozen: Vec<u8> = store::concat_shards(&stage, "fixture-machine", &shared_id).unwrap();

    // Each journal gains one more synthetic event; the scan now ids each
    // journal by its workflow run.
    for (_, journal, _, post_body) in &journals {
        fs::OpenOptions::new()
            .append(true)
            .open(journal)
            .unwrap()
            .write_all(post_body.as_bytes())
            .unwrap();
    }
    let post = scan_journals(&root);
    assert_eq!(post.records.len(), 3);
    let post_ids: std::collections::BTreeSet<String> = post
        .records
        .iter()
        .map(|record| record.id.as_str().to_string())
        .collect();
    let expected_post_ids: std::collections::BTreeSet<String> = journals
        .iter()
        .map(|(run, _, _, _)| format!("claude-code.fixture-machine.{run}"))
        .collect();
    assert_eq!(
        post_ids, expected_post_ids,
        "each journal ids by its workflow-run directory"
    );
    assert!(
        post.archive_gaps().is_empty(),
        "many distinct run ids are gap-free; gaps={:?}",
        post.archive_gaps()
    );

    let second =
        collect::collect_scan_report(&post, &stage, "fixture-machine", &state, 20, &dest())
            .unwrap();
    assert!(second.errors.is_empty());
    assert!(second.archive_gaps.is_empty());
    assert_eq!(
        second.reset_records, 0,
        "every per-path cursor proved itself against the old shared slot: no full reread"
    );
    assert_eq!(second.changed_records, 3);
    let expected_delta: u64 = journals
        .iter()
        .map(|(_, _, _, post_body)| post_body.len() as u64)
        .sum();
    assert_eq!(
        second.delta_bytes_read, expected_delta,
        "only the appended bytes were read"
    );
    assert_eq!(second.unchanged_records, 0);

    // The frozen merged slot: byte-identical, still exactly its three
    // pre-upgrade shards.
    assert_eq!(
        sealed_shard_bodies(&stage, "fixture-machine", &shared_id),
        expected_pre_bodies,
        "the merged slot is frozen: no run's delta was appended to it"
    );
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &shared_id).unwrap(),
        frozen,
        "the merged slot was not rewritten"
    );
    // Each run's own slot: exactly the post-upgrade bytes, once — the
    // exact shard-body equality above is also the no-duplication proof,
    // because a reread cursor would have sealed the pre-upgrade prefix
    // there too.
    for (run, _, _, post_body) in &journals {
        let run_id = format!("claude-code.fixture-machine.{run}");
        assert_eq!(
            sealed_shard_bodies(&stage, "fixture-machine", &run_id),
            vec![post_body.as_bytes().to_vec()],
            "run {run}'s slot holds exactly its post-upgrade bytes"
        );
    }
    // Nothing went anywhere else: the machine partition holds exactly the
    // frozen slot and the three per-run slots.
    assert_eq!(
        staged_session_slots(&stage, "fixture-machine"),
        journals
            .iter()
            .map(|(run, _, _, _)| format!("claude-code.fixture-machine.{run}"))
            .chain([shared_id.clone()])
            .collect(),
        "no record was duplicated or lost across slots"
    );

    // A pass that changed nothing adds nothing anywhere.
    let third = collect::collect_scan_report(&post, &stage, "fixture-machine", &state, 20, &dest())
        .unwrap();
    assert_eq!(third.changed_records, 0);
    assert_eq!(third.unchanged_records, 3);
    assert_eq!(third.delta_bytes_read, 0);
    assert_eq!(third.reset_records, 0);
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &shared_id).unwrap(),
        frozen,
        "a re-run leaves the merged slot frozen"
    );
    for (run, _, _, post_body) in &journals {
        assert_eq!(
            store::concat_shards(&stage, "fixture-machine", &run_id(run)).unwrap(),
            post_body.as_bytes().to_vec(),
            "a re-run adds nothing to run {run}'s slot"
        );
    }
}

/// GB-2, collect level (W980). The scan-only aliasing tests prove no gap
/// fires when two files resolve to one session id; this end-to-end pass
/// proves what that claim rests on: collect archives **both** paths' bytes
/// into the merged slot and drops nothing — if a future collect ever
/// treated the second same-id path as already covered, the missing body
/// fails here while no gap signal fires anywhere. Two aliasing shapes run
/// side by side: a workflow journal beside an ordinary transcript whose
/// filename stem spells the same run id (the collision the structural
/// journal id rule can itself produce), and the plain same-stem pair that
/// predates the rule. Per-path cursors keep each source distinct, so a
/// re-run adds nothing.
#[test]
fn aliased_session_id_pairs_collect_every_byte_into_the_merged_slots() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("projects");
    let run = "wf_019d7g00-0000-7000-8000-00000000000a";
    let journal_body = &b"{\"synthetic\":\"journal-body\"}\n"[..];
    let transcript_body = &b"{\"synthetic\":\"transcript-body\"}\n"[..];
    let dup_body_a = &b"{\"synthetic\":\"dup-a\"}\n"[..];
    let dup_body_b = &b"{\"synthetic\":\"dup-b\"}\n"[..];
    let files = [
        // An ordinary session transcript whose filename stem is the same
        // string as the workflow run id: it and the journal resolve to one
        // session id.
        (
            format!("-fixture-proj-a/{run}.jsonl"),
            transcript_body.to_vec(),
        ),
        (
            format!(
                "-fixture-proj-a/019d7g01-0000-7000-8000-000000000001/subagents/workflows/{run}/journal.jsonl"
            ),
            journal_body.to_vec(),
        ),
        // The plain same-stem pair: one stem in two project directories.
        (
            "-fixture-proj-b/dup-stem.jsonl".to_string(),
            dup_body_a.to_vec(),
        ),
        (
            "-fixture-proj-c/dup-stem.jsonl".to_string(),
            dup_body_b.to_vec(),
        ),
    ];
    for (rel, body) in &files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
    }
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");

    let report = scan_journals(&root);
    assert_eq!(report.records.len(), 4, "every aliased file is a record");
    let scanned_ids: std::collections::BTreeSet<String> = report
        .records
        .iter()
        .map(|record| record.id.as_str().to_string())
        .collect();
    let collided_id = run_id(run);
    let dup_id = "claude-code.fixture-machine.dup-stem".to_string();
    assert_eq!(
        scanned_ids,
        [collided_id.clone(), dup_id.clone()].into_iter().collect(),
        "two aliased pairs, each resolving to one session id"
    );
    assert_eq!(
        report.probes[0].record_count,
        Some(2),
        "the recognized count is the distinct ids collect can address"
    );
    assert!(
        report.archive_gaps().is_empty(),
        "aliasing is not a gap: collect consumes every record; gaps={:?}",
        report.archive_gaps()
    );

    let collected =
        collect::collect_scan_report(&report, &stage, "fixture-machine", &state, 20, &dest())
            .unwrap();
    assert!(collected.errors.is_empty());
    assert!(collected.archive_gaps.is_empty());
    assert_eq!(
        collected.changed_records, 4,
        "every file delivered its bytes this pass"
    );

    let mut expected_collided = vec![journal_body.to_vec(), transcript_body.to_vec()];
    expected_collided.sort();
    let mut collided = sealed_shard_bodies(&stage, "fixture-machine", &collided_id);
    collided.sort();
    assert_eq!(
        collided, expected_collided,
        "the collided run's merged slot holds the journal and the transcript, each exactly once"
    );
    let mut expected_dup = vec![dup_body_a.to_vec(), dup_body_b.to_vec()];
    expected_dup.sort();
    let mut dup = sealed_shard_bodies(&stage, "fixture-machine", &dup_id);
    dup.sort();
    assert_eq!(
        dup, expected_dup,
        "the dup-stem merged slot holds both files' bodies, each exactly once"
    );
    let collided_concat = store::concat_shards(&stage, "fixture-machine", &collided_id).unwrap();
    assert!(
        collided_concat == [journal_body, transcript_body].concat()
            || collided_concat == [transcript_body, journal_body].concat(),
        "the merged slot reads back both bodies exactly once"
    );
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &dup_id)
            .unwrap()
            .len(),
        dup_body_a.len() + dup_body_b.len(),
        "nothing was dropped from the dup-stem slot"
    );
    // No byte went anywhere else: the machine partition holds exactly the
    // two merged slots.
    assert_eq!(
        staged_session_slots(&stage, "fixture-machine"),
        [collided_id.clone(), dup_id.clone()].into_iter().collect(),
        "neither pair member staged a slot of its own"
    );

    // A pass that changed nothing adds nothing: each path kept its own
    // cursor, so no file of a pair hides behind its same-id sibling.
    let second =
        collect::collect_scan_report(&report, &stage, "fixture-machine", &state, 20, &dest())
            .unwrap();
    assert_eq!(second.changed_records, 0);
    assert_eq!(second.unchanged_records, 4);
    assert_eq!(second.delta_bytes_read, 0);
    assert_eq!(second.reset_records, 0);
    let mut collided_again = sealed_shard_bodies(&stage, "fixture-machine", &collided_id);
    collided_again.sort();
    assert_eq!(collided_again, expected_collided);
    let mut dup_again = sealed_shard_bodies(&stage, "fixture-machine", &dup_id);
    dup_again.sort();
    assert_eq!(dup_again, expected_dup);
    assert_eq!(
        staged_session_slots(&stage, "fixture-machine"),
        [collided_id, dup_id].into_iter().collect()
    );
}
