//! W880 — per-phase timers and counters for one `run-once` pass.
//!
//! Two angles, both on synthetic fixtures only:
//!
//! 1. the collector's own record (`CollectReport`) carries the
//!    collect-phase counters, asserted against a synthetic source
//!    tree whose bytes are known;
//! 2. the real binary's `run-once` prints the one summary line and
//!    records the same numbers in `run-state.json`, asserted against
//!    a sandboxed pass that collects one synthetic session and
//!    pushes it to a sandbox repository.
//!
//! Assertions inspect counts and byte totals only; no conversation
//! text appears in any assertion or output.

use chat_stasher::collect;
use chat_stasher::collect::DestinationView;
use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry};
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

/// These tests only exercise the local read path, so the destination
/// archive is deliberately not reachable.
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

/// The collector's counters over a source tree whose bytes are known.
///
/// First pass: no stored cursor, so the read path stats the source
/// twice (size before the read, size after it) and validates no
/// committed prefix. Second pass: a stored cursor whose committed
/// prefix is re-read and re-hashed, so the validated bytes appear
/// in `shard_bytes_read_hashed`.
#[test]
fn collect_report_carries_the_phase_counters() {
    let dir = tempfile::tempdir().unwrap();
    let source_root = dir.path().join("source");
    fs::create_dir_all(&source_root).unwrap();
    let source = source_root.join("session.jsonl");
    let first_bytes = b"one\ntwo\nthree\n";
    fs::write(&source, first_bytes).unwrap();
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
    assert_eq!(first.scanned_records, 1);
    assert_eq!(
        first.files_statted, 2,
        "the read path stats the source twice"
    );
    assert_eq!(first.source_bytes_read, first_bytes.len() as u64);
    assert_eq!(
        first.shard_bytes_read_hashed, 0,
        "a first read validates no committed prefix"
    );
    assert_eq!(first.state_saves, 1, "one cursor was durably saved");
    assert_eq!(first.sqlite_sessions_queried, 0);
    assert_eq!(first.sqlite_sessions_exported, 0);
    assert_eq!(
        first.collect_harness_ms.keys().collect::<Vec<_>>(),
        ["claude-code"],
        "the record's harness is bucketed by its id"
    );

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
    assert_eq!(second.scanned_records, 1);
    assert_eq!(second.files_statted, 2);
    assert_eq!(second.source_bytes_read, b"four\nfive\n".len() as u64);
    assert_eq!(
        second.shard_bytes_read_hashed,
        first_bytes.len() as u64,
        "the committed prefix was re-read and re-hashed"
    );
    assert_eq!(second.state_saves, 1, "the advanced cursor was saved once");
    assert_eq!(second.sqlite_sessions_queried, 0);
    assert_eq!(second.sqlite_sessions_exported, 0);
}

// ---------------------------------------------------------------------------
// The real binary: one summary line, and the record it leaves behind.
// ---------------------------------------------------------------------------

/// Run the real binary with every ambient path redirected into
/// `sandbox`, against a registry whose only harness is a synthetic
/// claude-code cell.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    if !registry.exists() {
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
        fs::write(
            &registry,
            serde_json::to_string(&json!({
                "schema_version": 1,
                "generated": "W880 synthetic",
                "harnesses": [{
                    "id": "claude-code",
                    "display_name": "synthetic",
                    "paths": paths
                }]
            }))
            .unwrap(),
        )
        .unwrap();
    }
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("rh-cache"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

/// Where `collect::default_state_dir()` lands given the `XDG_DATA_HOME`
/// above.
fn state_dir(sandbox: &Path) -> PathBuf {
    sandbox.join("data").join("chat-stasher").join("state")
}

/// A `run-once` pass over one synthetic session prints the phase
/// summary line and leaves a `run-state.json` whose `phases` carry
/// the same counts the pass measured.
#[test]
fn run_once_prints_the_summary_line_and_records_the_phases() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let home = sandbox.join("home");
    let projects = home.join(".claude").join("projects");
    fs::create_dir_all(&projects).unwrap();
    let session = projects.join("session.jsonl");
    let bytes = b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n";
    fs::write(&session, bytes).unwrap();
    let stage = sandbox.join("stage");

    let out = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "w880-machine",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let all = format!("{stdout}\n{stderr}");
    assert!(out.status.success(), "run-once failed: {all}");
    assert!(
        stdout.contains("result: COMPLETED"),
        "the pass must complete a snapshot: {all}"
    );
    // The one summary line, on stdout, carrying both halves.
    let summary = stdout
        .lines()
        .find(|line| line.starts_with("[run-once] phases ms:"))
        .expect("run-once must print the phase summary line");
    for key in [
        "scan=",
        "collect=",
        "stage_audit=",
        "metadata_hash=",
        "activity_index=",
        "push_preflight=",
        "backup=",
        "run_state_write=",
        "records_scanned=",
        "files_statted=",
        "source_bytes_read=",
        "shard_bytes_read_hashed=",
        "sqlite_sessions_queried=",
        "sqlite_sessions_exported=",
        "state_saves=",
    ] {
        assert!(
            summary.contains(key),
            "summary line must carry {key}: {summary}"
        );
    }

    // The durable record carries the same numbers.
    let raw = fs::read_to_string(state_dir(sandbox).join("run-state.json"))
        .expect("run-state.json must exist");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("run-state.json must parse");
    assert_eq!(value["outcome"], "completed");
    let phases = &value["phases"];
    assert_eq!(phases["records_scanned"].as_u64(), Some(1));
    assert_eq!(
        phases["source_bytes_read"].as_u64(),
        Some(bytes.len() as u64)
    );
    assert!(
        phases["files_statted"].as_u64().is_some_and(|n| n >= 1),
        "the pass stat'ed the source: {phases}"
    );
    assert!(
        phases["state_saves"].as_u64().is_some_and(|n| n >= 1),
        "the pass saved collector state: {phases}"
    );
    assert!(
        phases["shard_bytes_read_hashed"]
            .as_u64()
            .is_some_and(|n| n >= 1),
        "the activity index read and hashed the sealed shard body: {phases}"
    );
    assert_eq!(phases["sqlite_sessions_queried"].as_u64(), Some(0));
    assert_eq!(phases["sqlite_sessions_exported"].as_u64(), Some(0));
    let harness_ms = phases["collect_harness_ms"]
        .as_object()
        .expect("per-harness collect timings are an object");
    assert!(
        harness_ms.contains_key("claude-code"),
        "the claude-code harness is bucketed: {harness_ms:?}"
    );

    // A second pass with nothing new is a no-op whose record still
    // carries the phases — including the run-state write's own time.
    let out2 = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "w880-machine",
        ],
    );
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    let all2 = format!("{stdout2}\n{stderr2}");
    assert!(out2.status.success(), "second run-once failed: {all2}");
    assert!(
        stdout2.contains("result: NOOP"),
        "the second pass must be a no-op: {all2}"
    );
    assert!(
        stdout2.contains("[run-once] phases ms:"),
        "the second pass prints the summary line too: {all2}"
    );
    let raw2 = fs::read_to_string(state_dir(sandbox).join("run-state.json"))
        .expect("run-state.json must still exist");
    let value2: serde_json::Value = serde_json::from_str(&raw2).expect("run-state.json must parse");
    assert_eq!(value2["outcome"], "noop");
    assert!(
        value2["phases"]["run_state_write_ms"].as_u64().is_some(),
        "the no-op record still carries the run-state write time: {value2}"
    );
}

/// `status --json` exposes the per-phase record of the last pass.
#[test]
fn status_json_exposes_the_last_pass_phases() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let home = sandbox.join("home");
    let projects = home.join(".claude").join("projects");
    fs::create_dir_all(&projects).unwrap();
    fs::write(projects.join("session.jsonl"), b"{\"type\":\"user\"}\n").unwrap();
    let stage = sandbox.join("stage");

    let out = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "w880-machine",
        ],
    );
    assert!(
        out.status.success(),
        "run-once failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let status = run(sandbox, &["status", "--json"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status --json must parse");
    let phases = &value["run_state"]["phases"];
    assert!(
        phases["records_scanned"].as_u64().is_some_and(|n| n >= 1),
        "status --json must expose the last pass's phase record: {value}"
    );
    assert!(
        phases["collect_harness_ms"].as_object().is_some(),
        "status --json must expose the per-harness timings: {value}"
    );
}
