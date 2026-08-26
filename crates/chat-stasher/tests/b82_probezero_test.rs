//! B82 — "I could not confirm" must never be printed as "confirmed nothing".
//!
//! Four reporting paths turned an unfinished look into a measured zero:
//!
//!   * A2 — a store root whose `metadata` call failed, or that exists as the
//!     wrong kind of thing, became `ProbeState::Missing` / `不存在`, and every
//!     non-`FileTarget` probe printed `会话=0` in the doctor table.
//!   * A3 — `status` derived "本机没有扫描到任何会话" from `records.is_empty()`
//!     alone, ignoring the harnesses this run never got to look at.
//!   * A4 — Cursor's legacy `workspaceStorage` walk counted its own ignorance
//!     and then discarded it at the return, printing "未找到可读 composer 数据".
//!   * A5 — an unreadable collector record arrived at `dest-init` as `false`
//!     and was then used as *evidence* that a destination was never built.
//!
//! Unreadability is produced with filesystem permissions, never by editing the
//! code under test. Every CLI invocation runs against a temporary HOME/XDG
//! tree and a scratch registry; the real harnesses, archives and remotes are
//! never touched, and no session body is ever read.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::json;
use sha2::{Digest, Sha256};

/// A machine that was fully looked at and holds one session. Pinned so that
/// fixing the four lies above cannot move one byte of a healthy machine's
/// output. Same value as B81's guard — the two agree on purpose.
const CLEAN_SESSION_STATUS_BODY_SHA256: &str =
    "e9496f26534d12d97c2728b2b69f6d6cb6760c3a20805ee586e6b1c108483b77";

/// A machine that was fully looked at and genuinely holds nothing. This is the
/// exact sentence A3 qualifies, so its *un*qualified form has to be pinned
/// too: "本机没有扫描到任何会话。" is still the right answer when every probe
/// really did look.
const CLEAN_EMPTY_STATUS_BODY_SHA256: &str =
    "b0525f381aee4ba2a2db6d85fe755c338bd59358590809e80d666c09c3fe5a49";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn write_registry(sandbox: &Path, harnesses: serde_json::Value) -> PathBuf {
    let registry = json!({
        "schema_version": 1,
        "generated": "B82",
        "harnesses": harnesses
    });
    let path = sandbox.join("registry.json");
    fs::write(
        &path,
        serde_json::to_vec(&registry).expect("serialize scratch registry"),
    )
    .expect("write scratch registry");
    path
}

fn jsonl_harness(root: &Path) -> serde_json::Value {
    let cell = json!({
        "template": root,
        "format": "jsonl",
        "session_pattern": "*.jsonl",
        "confidence": "本机实测",
        "source": "B82 test fixture"
    });
    json!([{
        "id": "claude-code",
        "display_name": "Claude Code",
        "paths": {"macos": cell, "linux": cell, "windows": cell}
    }])
}

fn single_file_harness(db: &Path) -> serde_json::Value {
    let cell = json!({
        "template": db,
        "format": "sqlite",
        "confidence": "本机实测",
        "source": "B82 test fixture",
        "sql_table": "session",
        "sql_required_columns": ["id", "time_created", "time_updated"],
        "sql_id_column": "id",
        "sql_time_column": "time_updated"
    });
    json!([{
        "id": "opencode",
        "display_name": "opencode",
        "paths": {"macos": cell, "linux": cell, "windows": cell}
    }])
}

fn cursor_harness(global_db: &Path) -> serde_json::Value {
    let cell = json!({
        "template": global_db,
        "format": "sqlite",
        "confidence": "本机实测",
        "source": "B82 test fixture",
        "sql_table": "cursorDiskKV",
        "sql_required_columns": ["key", "value"],
        "sql_key_column": "key",
        "sql_key_pattern": "composerData:%",
        "sql_value_column": "value",
        "sql_qualification": "cursor_composer"
    });
    json!([{
        "id": "cursor",
        "display_name": "Cursor",
        "paths": {"macos": cell, "linux": cell, "windows": cell}
    }])
}

fn run_cli(sandbox: &Path, registry: &Path, command: &str) -> Output {
    run_cli_args(sandbox, registry, &[command.to_string()])
}

fn run_cli_args(sandbox: &Path, registry: &Path, args: &[String]) -> Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).expect("create isolated home");
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("CHAT_STASHER_REGISTRY", registry)
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .output()
        .expect("run chat-stasher command")
}

fn status_body(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.starts_with("[run-once]"))
        .map(|line| format!("{line}\n"))
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    let mut permissions = fs::metadata(path).expect("stat fixture path").permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions).expect("set fixture permissions");
}

/// A minimal opencode-shaped store, so the clean path is a real `FileTarget`
/// read and not another kind of failure.
fn plant_readable_store(db: &Path) -> PathBuf {
    fs::create_dir_all(db.parent().expect("store parent")).expect("create store directory");
    let conn = rusqlite::Connection::open(db).expect("create fixture store");
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, time_created INTEGER, time_updated INTEGER);",
    )
    .expect("create fixture schema");
    drop(conn);
    db.to_path_buf()
}

// ---------------------------------------------------------------------------
// A2 — a root we could not stat is not a root that is not there
// ---------------------------------------------------------------------------

/// Before B82: `fs::metadata` failing (here: EACCES on the parent directory)
/// fell into the catch-all `_ =>` arm and produced
/// `state=Missing, note="单文件不存在"`, the path went into `missing_roots`,
/// and the doctor table printed `不存在 … 会话=0`. Rolling the fix back makes
/// every assertion below fail on exactly that output.
#[cfg(unix)]
#[test]
fn a2_unstattable_single_file_root_is_not_reported_as_absent() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let blocked = sandbox.path().join("blocked");
    let db = blocked.join("store.db");
    plant_readable_store(&db);
    set_mode(&blocked, 0o0);
    let registry = write_registry(sandbox.path(), single_file_harness(&db));

    let status = run_cli(sandbox.path(), &registry, "status");
    let doctor = run_cli(sandbox.path(), &registry, "doctor");
    set_mode(&blocked, 0o755);

    let status_text = status_body(&status);
    let doctor_text = String::from_utf8_lossy(&doctor.stderr).into_owned();
    let row = probe_row(&doctor_text, "opencode");

    assert!(
        status_text.contains("存在与否未知"),
        "status must not silently drop a root it could not stat: {status_text}"
    );
    assert!(
        !status_text.contains("不存在的来源根目录"),
        "an unstattable root must not be counted as a missing one: {status_text}"
    );
    assert!(
        row.contains("indeterminate") && row.contains("sessions=unknown"),
        "doctor must print unknown, not a fabricated 0, for a root it could not stat: {row}"
    );
    assert!(
        !row.contains("单文件不存在") && !row.starts_with("    missing"),
        "doctor must not call an unstattable root absent: {row}"
    );
}

/// The other half of A2's first clause: the path resolves, exists, and is the
/// wrong kind of thing. `metadata` succeeds, `md.is_file()` is false, and the
/// old catch-all reported "单文件不存在" about a path that is plainly there.
#[test]
fn a2_wrong_path_type_is_reported_as_unknown_not_absent() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let db = sandbox.path().join("store.db");
    fs::create_dir_all(&db).expect("plant a directory where a file is declared");
    let registry = write_registry(sandbox.path(), single_file_harness(&db));

    let doctor = run_cli(sandbox.path(), &registry, "doctor");
    let doctor_text = String::from_utf8_lossy(&doctor.stderr).into_owned();
    let row = probe_row(&doctor_text, "opencode");

    assert!(
        row.contains("路径存在但不是文件") && row.contains("sessions=unknown"),
        "a path of the wrong type is unknown, not empty: {row}"
    );
    assert!(
        !row.contains("单文件不存在"),
        "a path that exists must never be reported as absent: {row}"
    );
}

// ---------------------------------------------------------------------------
// A3 — "本机没有扫描到任何会话" is a claim about the machine, not about records
// ---------------------------------------------------------------------------

/// Before B82 this printed the bare sentence "本机没有扫描到任何会话。" while
/// one of the two harnesses had never been looked at at all (its template does
/// not reduce to a static root). Zero records plus places we did not look is
/// not "there is nothing here".
#[test]
fn a3_status_qualifies_its_zero_when_a_harness_was_never_looked_at() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let absent = sandbox.path().join("no-such-root");
    let unresolvable = json!({
        "template": "$CWD/.crush/crush.db",
        "format": "sqlite",
        "confidence": "本机实测",
        "source": "B82 test fixture"
    });
    let present = json!({
        "template": absent,
        "format": "jsonl",
        "session_pattern": "*.jsonl",
        "confidence": "本机实测",
        "source": "B82 test fixture"
    });
    let registry = write_registry(
        sandbox.path(),
        json!([
            {
                "id": "claude-code",
                "display_name": "Claude Code",
                "paths": {"macos": present, "linux": present, "windows": present}
            },
            {
                "id": "crush",
                "display_name": "Crush",
                "paths": {"macos": unresolvable, "linux": unresolvable, "windows": unresolvable}
            }
        ]),
    );

    let status = run_cli(sandbox.path(), &registry, "status");
    let status_text = status_body(&status);

    assert!(
        status_text.contains("本机没有扫描到任何会话"),
        "the established sentence must still be there: {status_text}"
    );
    assert!(
        status_text.contains("根本没查") && status_text.contains("crush"),
        "a machine-wide zero must name the harnesses this run never looked at: {status_text}"
    );

    // `collect` reports the same coverage gap on its own partial-scan line,
    // and still runs: this is a fact about coverage, not a reason to stop.
    let collect = run_cli_args(
        sandbox.path(),
        &registry,
        &[
            "collect".to_string(),
            "--stage".to_string(),
            sandbox.path().join("stage").display().to_string(),
            "--machine".to_string(),
            "b82-probezero".to_string(),
        ],
    );
    let collect_text = String::from_utf8_lossy(&collect.stdout).into_owned();
    assert_eq!(collect.status.code(), Some(0));
    assert!(
        collect_text.contains("scan partial") && collect_text.contains("unlooked_harnesses=1"),
        "collect must say which harnesses it never looked at: {collect_text}"
    );
}

// ---------------------------------------------------------------------------
// A4 — Cursor's legacy walk counted its ignorance and then threw it away
// ---------------------------------------------------------------------------

/// `workspaceStorage` exists and cannot be enumerated (EACCES). Before B82 the
/// walk's `fs::read_dir(...).ok()?` turned that into `None`, and the scanner
/// printed "globalStorage/state.vscdb 不存在，legacy workspaceStorage 也未找到
/// 可读 composer 数据" — an absence claim built out of a directory nobody was
/// allowed to open.
#[cfg(unix)]
#[test]
fn a4_unenumerable_cursor_legacy_storage_is_not_reported_as_no_data() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let user = sandbox.path().join("Cursor/User");
    let workspace_storage = user.join("workspaceStorage");
    fs::create_dir_all(workspace_storage.join("ws-1")).expect("create legacy workspace");
    let global_db = user.join("globalStorage/state.vscdb");
    fs::create_dir_all(global_db.parent().expect("globalStorage parent"))
        .expect("create globalStorage");
    set_mode(&workspace_storage, 0o0);
    let registry = write_registry(sandbox.path(), cursor_harness(&global_db));

    let status = run_cli(sandbox.path(), &registry, "status");
    let doctor = run_cli(sandbox.path(), &registry, "doctor");
    set_mode(&workspace_storage, 0o755);

    let status_text = status_body(&status);
    let doctor_text = String::from_utf8_lossy(&doctor.stderr).into_owned();
    let row = probe_row(&doctor_text, "Cursor");

    assert!(
        row.contains("不能据此说没有") && row.contains("sessions=unknown"),
        "an un-enumerable legacy store is unknown, not empty: {row}"
    );
    assert!(
        !row.contains("未找到可读 composer 数据"),
        "the 'no readable composer data' sentence requires a completed walk: {row}"
    );
    assert!(
        status_text.contains("会话数未知"),
        "status must carry the unknown legacy cardinality: {status_text}"
    );
}

/// The `sqlite_probe` half of the same fix, at the level where the counting
/// happens: a workspace database that refuses to open is counted, and that
/// count now leaves the function instead of dying with the `Option`.
#[cfg(unix)]
#[test]
fn a4_unreadable_workspace_database_leaves_the_probe_as_ignorance() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let workspace_storage = sandbox.path().join("workspaceStorage");
    let db = workspace_storage.join("ws-1/state.vscdb");
    fs::create_dir_all(db.parent().expect("workspace parent")).expect("create workspace");
    fs::write(&db, b"not a database").expect("write unreadable workspace database");
    set_mode(&db, 0o0);

    let scan =
        chat_stasher::sqlite_probe::probe_cursor_legacy_workspace_storage(&workspace_storage);
    set_mode(&db, 0o644);

    assert!(
        scan.probe.is_none(),
        "the enumeration itself is unchanged: nothing was decodable"
    );
    assert_eq!(
        scan.unreadable_stores, 1,
        "a store that will not open is one unreadable store, not zero sessions"
    );
    assert!(
        scan.saw_ignorance(),
        "the caller must be able to tell this apart from an empty workspaceStorage"
    );
}

// ---------------------------------------------------------------------------
// A5 — ignorance used as proof
// ---------------------------------------------------------------------------

/// The measurement the old API could not make. `destination_has_record`
/// answers `false` for an unreadable state file — exactly what it answers for
/// a destination we have genuinely never collected for — and `dest-init` then
/// spent that `false` as evidence of "never built".
#[cfg(unix)]
#[test]
fn a5_unreadable_collector_record_is_an_error_not_a_no() {
    let state_dir = tempfile::tempdir().expect("create state dir");
    let state = state_dir.path().join("debts-v2.json");
    fs::write(
        &state,
        br#"{"version":2,"destinations":{"dest-a":{"files":{}}}}"#,
    )
    .expect("write collector state");
    set_mode(&state, 0o0);

    let record = chat_stasher::collect::destination_record(state_dir.path(), "dest-a");
    let flattened = chat_stasher::collect::destination_has_record(state_dir.path(), "dest-a");
    set_mode(&state, 0o644);

    assert!(
        record.is_err(),
        "an unreadable record must not answer the question at all: {record:?}"
    );
    assert!(
        !flattened,
        "this is the old answer, and it is indistinguishable from 'never collected' \
         — which is why the boolean must not decide anything"
    );
    // The *reachable* form of the same fault, and the one that made it all the
    // way to `dest-init` exiting 0: `load_state` deliberately maps a
    // version-mismatched file to an empty state so the collector re-reads
    // everything, and the old `destination_has_record` inherited that empty
    // state as a confident "no".
    let stale = tempfile::tempdir().expect("create stale state dir");
    fs::write(
        stale.path().join("debts-v2.json"),
        br#"{"version":99,"destinations":{}}"#,
    )
    .expect("write version-mismatched collector state");
    assert!(
        chat_stasher::collect::destination_record(stale.path(), "dest-a").is_err(),
        "a state file this build cannot interpret answers nothing about any destination"
    );

    // A state file that is simply not there is still a real answer.
    let empty = tempfile::tempdir().expect("create empty state dir");
    assert_eq!(
        chat_stasher::collect::destination_record(empty.path(), "dest-a").unwrap(),
        chat_stasher::collect::DestinationRecord::Unrecorded
    );
}

/// The decision that boolean fed. Same absent repository, same everything —
/// only the record differs. `Absent` (what the old flattening always produced)
/// still exits through `KnownEmpty`; `Unreadable` must not, because
/// `KnownEmpty` is the one absence that keeps `diff_complete` true and lets the
/// run be declared COMPLETE.
#[test]
fn a5_unreadable_record_downgrades_never_built_to_unknown() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let stage = sandbox.path().join("stage");
    fs::create_dir_all(&stage).expect("create stage");
    let absent_repo = sandbox.path().join("no-such-destination");

    let verdict = |record| {
        let sources = vec![chat_stasher::destinit::SourceDestination {
            name: "peer".to_string(),
            cfg: chat_stasher::store::StoreConfig {
                repo_root: absent_repo.display().to_string(),
                key_file: sandbox.path().join("peer.key"),
                connections: 1,
                options: std::collections::BTreeMap::new(),
            },
            record,
        }];
        chat_stasher::destinit::fill_difference(&stage, "b82-probezero", 1, &sources)
    };

    let absent = verdict(chat_stasher::destinit::CollectRecord::Absent);
    assert_eq!(
        absent.sources[0].status,
        chat_stasher::destinit::SourceStatus::KnownEmpty,
        "the established never-built path must be untouched"
    );
    assert!(
        absent.diff_complete,
        "a known empty source proves the union"
    );

    let unreadable = verdict(chat_stasher::destinit::CollectRecord::Unreadable);
    assert_eq!(
        unreadable.sources[0].status,
        chat_stasher::destinit::SourceStatus::Unknown,
        "a missing repository plus a record we could not read is UNKNOWN, not never-built"
    );
    assert!(
        !unreadable.diff_complete,
        "an unproven union must not be reported as complete (dest-init exits 3 on this)"
    );
    assert!(
        unreadable.sources[0].record_unreadable,
        "the reason for the UNKNOWN must survive to the report"
    );
}

// ---------------------------------------------------------------------------
// The guard: none of the above may cost a healthy machine one byte
// ---------------------------------------------------------------------------

/// A store that reads cleanly must produce exactly the bytes and exit code it
/// produced before B82 — both when it holds sessions and when it holds none.
/// The false-positive guard is the second half: the same fixture made
/// unreadable has to hash differently, otherwise this test would pass even if
/// `status` had stopped saying anything at all.
#[cfg(unix)]
#[test]
fn a_clean_scan_keeps_status_bytes_and_exit_code_identical() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let db = sandbox.path().join("store/store.db");
    plant_readable_store(&db);
    let registry = write_registry(sandbox.path(), single_file_harness(&db));

    let output = run_cli(sandbox.path(), &registry, "status");
    let body = status_body(&output);

    assert_eq!(output.status.code(), Some(1), "run-once verdict unchanged");
    assert!(
        !body.contains("存在与否未知"),
        "clean scan says nothing new"
    );
    assert!(!body.contains("根本没查"), "clean scan says nothing new");
    assert_eq!(
        sha256_hex(body.as_bytes()),
        CLEAN_EMPTY_STATUS_BODY_SHA256,
        "a clean empty scan changed status bytes: {body}"
    );

    // A clean scan that finds something, byte for byte.
    let source = sandbox.path().join("source");
    fs::create_dir_all(&source).expect("create clean source root");
    fs::write(source.join("session.jsonl"), b"fixture\n").expect("write clean session fixture");
    let jsonl_registry = write_registry(sandbox.path(), jsonl_harness(&source));
    let with_sessions = run_cli(sandbox.path(), &jsonl_registry, "status");
    let with_sessions_body = status_body(&with_sessions);
    assert_eq!(with_sessions.status.code(), Some(1));
    assert_eq!(
        sha256_hex(with_sessions_body.as_bytes()),
        CLEAN_SESSION_STATUS_BODY_SHA256,
        "a clean scan with sessions changed status bytes: {with_sessions_body}"
    );

    // False-positive guard: the pinned hashes must be sensitive to the fixture.
    let blocked = sandbox.path().join("store");
    set_mode(&blocked, 0o0);
    let broken = run_cli(sandbox.path(), &registry, "status");
    set_mode(&blocked, 0o755);
    let broken_body = status_body(&broken);
    assert_ne!(
        sha256_hex(broken_body.as_bytes()),
        CLEAN_EMPTY_STATUS_BODY_SHA256,
        "the pinned hash must actually be sensitive to the fixture: {broken_body}"
    );
}

/// The doctor rows of a clean single-file scan, likewise pinned. This is the
/// table A2 changed, so its unchanged half has to be nailed down too.
#[test]
fn a_clean_scan_keeps_the_doctor_probe_row_identical() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let db = sandbox.path().join("store/store.db");
    plant_readable_store(&db);
    let registry = write_registry(sandbox.path(), single_file_harness(&db));

    let doctor = run_cli(sandbox.path(), &registry, "doctor");
    let doctor_text = String::from_utf8_lossy(&doctor.stderr).into_owned();
    let row = probe_row(&doctor_text, "opencode");

    assert_eq!(doctor.status.code(), Some(0), "doctor exit code unchanged");
    assert!(
        row.contains("single-file") && row.contains("sessions=0"),
        "a store that was read and holds nothing still prints 0: {row}"
    );
    assert!(
        !row.contains("unknown"),
        "a completed read must not pick up the new hedge: {row}"
    );
}

/// The third label in the doctor's session column. A harness the registry
/// does not list for this platform has nothing here to have missed, so it
/// prints `N/A` — distinct from both the `0` of a measured absence and the
/// `未知` of a look that did not happen.
#[cfg(unix)]
#[test]
fn a2_doctor_session_column_is_three_valued() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let cell = json!({
        "template": "C:\\fixture\\store.db",
        "format": "sqlite",
        "confidence": "本机实测",
        "source": "B82 test fixture"
    });
    let registry = write_registry(
        sandbox.path(),
        json!([{
            "id": "grok",
            "display_name": "Grok",
            "paths": {"windows": cell}
        }]),
    );

    let doctor = run_cli(sandbox.path(), &registry, "doctor");
    let doctor_text = String::from_utf8_lossy(&doctor.stderr).into_owned();
    let row = probe_row(&doctor_text, "Grok");

    assert!(
        row.contains("cross-platform") && row.contains("sessions=N/A"),
        "a harness with no cell for this platform is N/A, not 0: {row}"
    );
}

/// The `[registry]` table row for one harness, by display name.
fn probe_row(doctor_text: &str, display_name: &str) -> String {
    doctor_text
        .lines()
        .find(|line| line.contains(display_name) && line.contains("sessions="))
        .unwrap_or_else(|| panic!("no registry probe row for {display_name}:\n{doctor_text}"))
        .to_string()
}
