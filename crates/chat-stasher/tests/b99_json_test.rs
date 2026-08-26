//! B99 — `--json` output contract for `status` / `overview` / `doctor`.
//!
//! The three commands gain a `--json` flag whose contract is: stdout carries
//! exactly one JSON object and nothing else; every "unknown" is an explicit
//! tagged shape (`{"kind":"unknown","why":…}`), never `0`/`null`/a missing
//! field; and **the exit code is unchanged by the flag**. These tests pin the
//! schema's field names (the repo's `clean_status_output_is_byte_identical`
//! precedent, applied to JSON) and the tri-state shapes through the real
//! binary.
//!
//! `overview` needs a destination repository for its success shape; that is
//! covered by the pure serialiser tests in `overview.rs` (`overview_json_*`).
//! Here `overview --json` is exercised only on the local-only error paths
//! (no destination / missing key), so no remote is ever contacted and no
//! repository is created.
//!
//! All fixtures are `tempfile` paths with isolated HOME/XDG variables; no real
//! harness directory, state dir, archive or remote is touched, and no session
//! body is read.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// The process environment is process-global and `cargo test` runs these tests
/// as threads of one process. Same lock the B85 suite needs: readers race
/// writers on `PATH`/`HOME`, and the spawned child snapshots the parent's
/// environment at spawn time.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn sandbox() -> tempfile::TempDir {
    tempfile::TempDir::new().unwrap()
}

/// Run the real binary with every ambient path redirected into `sandbox`.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    let registry = sandbox.join("registry.json");
    fs::write(
        &registry,
        br#"{"schema_version":1,"generated":"B99 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    let no_tools = sandbox.join("no-host-tools");
    fs::create_dir_all(&no_tools).unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("PATH", &no_tools)
        .env("HOSTNAME", "b99-fixture")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("XDG_CACHE_HOME", sandbox.join("xdg-cache"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .env_remove("CURSOR_USER_DIR")
        .output()
        .unwrap()
}

/// stdout parsed as a single JSON object, asserting the whole stream was one
/// value (no human text leaked onto stdout).
fn json_stdout(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<serde_json::Value>(&stdout).unwrap_or_else(|e| {
        panic!(
            "stdout must be exactly one JSON object, got {:?} ({e})",
            stdout
        )
    })
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "expected exit {code}, got {:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Write a `run-state.json` of a healthy, just-finished run into the sandbox's
/// state dir (`collect::default_state_dir()` under `XDG_DATA_HOME`).
fn write_healthy_run_state(sandbox: &Path) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let dir = sandbox.join("xdg-data/chat-stasher/state");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("run-state.json"),
        format!(
            r#"{{"version":1,"finished_at_unix":{now},"duration_ms":5,"outcome":"noop","failed_step":null,"shards_written":0,"stage_shards":0,"snapshot_created":false,"collect_errors":0,"archive_gaps":0,"machine_digest":"0123456789ab"}}"#
        ),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// `status --json`
// ---------------------------------------------------------------------------

/// Never ran: `run_state.kind` is `missing`, not a fabricated
/// `finished_at_unix`; the machine is unhealthy and exits 1. The flag must not
/// change the established exit code.
#[test]
fn status_json_never_ran_is_missing_and_unhealthy() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let sb = sandbox();
    let out = run(sb.path(), &["status", "--json"]);
    assert_exit(&out, 1);
    let v = json_stdout(&out);
    assert_eq!(v["schema_version"], serde_json::json!(1));
    assert_eq!(v["command"], serde_json::json!("status"));
    assert_eq!(v["healthy"], serde_json::json!(false));
    assert_eq!(v["exit_code"], serde_json::json!(1));
    assert!(v["exit_semantics"].is_string());
    assert_eq!(v["run_state"]["kind"], serde_json::json!("missing"));
    assert!(v["run_state"]["why"].is_string());
    assert_eq!(v["scanner"]["kind"], serde_json::json!("ok"));
    assert_eq!(
        v["scanner"]["total_sessions"],
        serde_json::json!({"kind":"known","count":0})
    );
}

/// A known, healthy run-state serialises as `run_state.kind == "known"` with a
/// numeric `finished_at_unix` (never null), `overdue` explicit, and exit 0.
#[test]
fn status_json_known_run_state_is_explicit() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let sb = sandbox();
    write_healthy_run_state(sb.path());
    let out = run(sb.path(), &["status", "--json"]);
    assert_exit(&out, 0);
    let v = json_stdout(&out);
    assert_eq!(v["healthy"], serde_json::json!(true));
    assert_eq!(v["run_state"]["kind"], serde_json::json!("known"));
    assert!(
        v["run_state"]["finished_at_unix"].is_u64(),
        "finished_at_unix must be a number, not null: {}",
        v["run_state"]
    );
    assert!(v["run_state"]["age_secs"].is_u64());
    assert!(v["run_state"]["overdue"].is_boolean());
    assert_eq!(v["run_state"]["outcome"], serde_json::json!("noop"));
}

/// A corrupt run-state file is `run_state.kind == "unreadable"`, never
/// `known` with a guessed time — and never exit 0.
#[test]
fn status_json_unreadable_run_state_is_unreadable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let sb = sandbox();
    let dir = sb.path().join("xdg-data/chat-stasher/state");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("run-state.json"), br#"{"version":1, garbage"#).unwrap();
    let out = run(sb.path(), &["status", "--json"]);
    assert_exit(&out, 1);
    let v = json_stdout(&out);
    assert_eq!(v["run_state"]["kind"], serde_json::json!("unreadable"));
    assert!(v["run_state"]["why"].is_string());
}

/// Top-level field names of `status --json` are pinned. Bumping one is a
/// breaking change for every tray-plugin script that parsed this object.
#[test]
fn status_json_top_level_schema_is_stable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let sb = sandbox();
    let out = run(sb.path(), &["status", "--json"]);
    assert_exit(&out, 1);
    let v = json_stdout(&out);
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "command",
            "config_source",
            "exit_code",
            "exit_semantics",
            "healthy",
            "run_state",
            "scanner",
            "schema_version",
        ]
    );
    // The scanner's aggregate counts are tri-state objects, never bare numbers.
    let scanner = v["scanner"].as_object().unwrap();
    for key in [
        "total_sessions",
        "compressed_sessions",
        "missing_roots",
        "indeterminate_roots",
        "archive_gap_harnesses",
        "unreadable_sessions",
        "unreadable_entries",
        "unreadable_uncounted_harnesses",
    ] {
        assert_eq!(
            scanner[key]["kind"],
            serde_json::json!("known"),
            "scanner.{key} must carry an explicit kind: {}",
            scanner[key]
        );
    }
}

/// `--json` must not leak a second object onto stdout even when the scan
/// fails: the `scanner` object becomes `{"kind":"failed","why":…}` and the
/// machine exits 3 (unchanged from the non-JSON path).
#[test]
fn status_json_scan_failure_is_kind_failed_not_zero() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let sb = sandbox();
    // Point the registry at a path that does not exist -> scan errors.
    let out = run_unreadable_registry(sb.path(), &["status", "--json"]);
    assert_exit(&out, 3);
    let v = json_stdout(&out);
    assert_eq!(v["exit_code"], serde_json::json!(3));
    assert_eq!(v["healthy"], serde_json::json!(false));
    assert_eq!(v["scanner"]["kind"], serde_json::json!("failed"));
    assert!(v["scanner"]["why"].is_string());
}

/// Same as [`run`] but with `CHAT_STASHER_REGISTRY` pointing at a missing
/// file, so the registry-driven scan fails instead of returning zero records.
fn run_unreadable_registry(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    let registry = sandbox.join("does-not-exist-registry.json");
    let no_tools = sandbox.join("no-host-tools");
    fs::create_dir_all(&no_tools).unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("PATH", &no_tools)
        .env("HOSTNAME", "b99-fixture")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("XDG_CACHE_HOME", sandbox.join("xdg-cache"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

// ---------------------------------------------------------------------------
// `doctor --json`
// ---------------------------------------------------------------------------

/// `doctor --json` prints one object and exits 0 on an empty machine, carrying
/// the same tri-state vocabulary; the flag does not change the exit code.
#[test]
fn doctor_json_prints_one_object_and_exit_0() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let sb = sandbox();
    let out = run(sb.path(), &["doctor", "--json"]);
    assert_exit(&out, 0);
    let v = json_stdout(&out);
    assert_eq!(v["command"], serde_json::json!("doctor"));
    assert_eq!(v["scan_failed"], serde_json::json!(false));
    assert_eq!(
        v["claude"]["verdict"]["kind"],
        serde_json::json!("unset_default")
    );
    assert_eq!(v["gemini"]["kind"], serde_json::json!("known"));
    assert_eq!(v["reclaim"]["kind"], serde_json::json!("no_repo"));
    // Every footprint count is a tri-state object, never a bare number or null.
    let footprint = &v["footprints"][0];
    assert_eq!(
        footprint["session_count"]["kind"],
        serde_json::json!("not_applicable")
    );
    assert_eq!(footprint["earliest"]["kind"], serde_json::json!("unknown"));
    // stdout was exactly one object — json_stdout already asserted it.
}

// ---------------------------------------------------------------------------
// `overview --json` (local-only paths; the success shape is pure-tested)
// ---------------------------------------------------------------------------

/// Usage error (no destination, no repo): one JSON object on stdout, exit 2.
#[test]
fn overview_json_no_destination_is_error_exit_2() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let sb = sandbox();
    let out = run(sb.path(), &["overview", "--json"]);
    assert_exit(&out, 2);
    let v = json_stdout(&out);
    assert_eq!(v["command"], serde_json::json!("overview"));
    assert_eq!(v["exit_code"], serde_json::json!(2));
    assert_eq!(v["healthy"], serde_json::json!(false));
    assert!(v["error"].is_string());
}

/// Unreadable archive (missing key): one JSON object on stdout, exit 3. "Could
/// not look" is `error`, not an empty `sessions` array that reads as "none".
#[test]
fn overview_json_unopenable_repo_is_error_exit_3() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let sb = sandbox();
    let out = run(
        sb.path(),
        &[
            "overview",
            "--json",
            "--repo",
            sb.path().join("nope-repo").to_str().unwrap(),
            "--key-file",
            sb.path().join("nope-key").to_str().unwrap(),
        ],
    );
    assert_exit(&out, 3);
    let v = json_stdout(&out);
    assert_eq!(v["exit_code"], serde_json::json!(3));
    assert_eq!(v["healthy"], serde_json::json!(false));
    assert!(v["error"].is_string());
    assert!(
        v.get("sessions").is_none(),
        "an unreadable overview must not claim an empty sessions list"
    );
}
