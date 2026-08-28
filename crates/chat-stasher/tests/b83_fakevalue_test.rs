//! B83 — a missing value must stay missing, not become a plausible value.
//!
//! These fixtures are synthetic and live only under `tempfile` directories.
//! The first three tests are deliberately written against the post-fix
//! contract: adding this file before the implementation change makes the old
//! checkout red, which is the rollback proof for D8/D9/D10.
//!
//! The normal ingest guard is a second, independent check. Its SHA-256 is the
//! captured pre-B83 stdout+stderr bytes, and its exit code is asserted beside
//! it. A false positive that merely silences output therefore cannot pass.

use chat_stasher::destinit::{self, CollectRecord, SourceDestination};
use chat_stasher::inbox;
use chat_stasher::sqlite_probe::{self, SqliteSchemaSpec};
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const MACHINE: &str = "b83-fixture";
const SESSION: &str = "b83.synthetic-session";

// Captured from the clean pre-B83 `ingest` path. Keep this literal: computing
// both sides from the same output would not guard the normal bytes.
const CLEAN_INGEST_OUTPUT_SHA256: &str =
    "1c417dd5b9f7c619f58d5ec552a64e33311137db97e46d85b7040e353480d99d";

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create synthetic parent");
    }
    fs::write(path, bytes).expect("write synthetic fixture");
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn first_shard_record(stage: &Path, id: &str) -> Value {
    let entries = store::sealed_shard_entries(&store::session_shard_dir(stage, MACHINE, id))
        .expect("list synthetic sealed shard");
    let (_, shard) = entries.first().expect("synthetic shard exists");
    let raw = fs::read_to_string(shard).expect("read synthetic shard metadata");
    let line = raw.lines().next().expect("synthetic shard has one line");
    serde_json::from_str(line).expect("parse synthetic shard metadata")
}

fn ingest_fixture(root: &Path, name: &str, bundle: &str) -> inbox::IngestReport {
    let inbox_root = root.join("inbox");
    let stage = root.join("stage");
    write(&inbox_root.join(name), bundle.as_bytes());
    inbox::ingest(&inbox_root, &stage, MACHINE).expect("ingest synthetic bundle")
}

/// D8 rollback proof and regression: before the fix, each missing envelope key
/// was accepted with a concrete fallback (`deepseek`, the filename, `false`,
/// an empty array, or zero bytes). Platform/session fallbacks are especially
/// unsafe because the composed id is also the stage path and dedup scope.
#[test]
fn d8_missing_bundle_fields_do_not_become_ids_or_empty_metadata() {
    let platform_missing = tempfile::tempdir().expect("create platform fixture");
    let report = ingest_fixture(
        platform_missing.path(),
        "foreign-session.json",
        r#"{"schema":"chat-stasher/inbox@1","sessionId":"b83-platform-session","parsed":{"hasJson":true,"keys":["id"]},"raw":{"text":"synthetic","bytes":9}}"#,
    );
    assert_eq!(
        report.consumed.len(),
        0,
        "missing platform must not be archived"
    );
    assert_eq!(
        report.errors.len(),
        1,
        "missing platform must remain an ingest error"
    );
    assert!(
        !store::session_shard_dir(
            &platform_missing.path().join("stage"),
            MACHINE,
            "deepseek.b83-platform-session"
        )
        .exists(),
        "missing platform must not enter the deepseek partition"
    );

    let session_missing = tempfile::tempdir().expect("create session fixture");
    let report = ingest_fixture(
        session_missing.path(),
        "filename-fallback.json",
        r#"{"schema":"chat-stasher/inbox@1","platform":"foreign-platform","parsed":{"hasJson":true,"keys":["id"]},"raw":{"text":"synthetic","bytes":9}}"#,
    );
    assert_eq!(
        report.consumed.len(),
        0,
        "missing sessionId must not be archived"
    );
    assert_eq!(
        report.errors.len(),
        1,
        "missing sessionId must remain an ingest error"
    );
    assert!(
        !store::session_shard_dir(
            &session_missing.path().join("stage"),
            MACHINE,
            "foreign-platform.filename-fallback"
        )
        .exists(),
        "missing sessionId must not enter the filename-derived id"
    );

    let parsed_missing = tempfile::tempdir().expect("create parsed fixture");
    let report = ingest_fixture(
        parsed_missing.path(),
        "parsed-missing.json",
        r#"{"schema":"chat-stasher/inbox@1","platform":"foreign-platform","sessionId":"b83-parsed-session","raw":{"text":"synthetic","bytes":9}}"#,
    );
    assert_eq!(
        report.consumed.len(),
        1,
        "parsed is best-effort and may be absent"
    );
    assert_eq!(
        report.errors.len(),
        0,
        "missing parsed must not be a read error"
    );
    let record = first_shard_record(
        &parsed_missing.path().join("stage"),
        "foreign-platform.b83-parsed-session",
    );
    assert!(
        record.get("parsed").is_none(),
        "missing parsed must stay unknown, not become false and []"
    );

    let raw_missing = tempfile::tempdir().expect("create raw fixture");
    let report = ingest_fixture(
        raw_missing.path(),
        "raw-missing.json",
        r#"{"schema":"chat-stasher/inbox@1","platform":"foreign-platform","sessionId":"b83-raw-session","parsed":{"hasJson":true,"keys":["id"]}}"#,
    );
    assert_eq!(
        report.consumed.len(),
        0,
        "missing raw must not archive an empty body"
    );
    assert_eq!(
        report.errors.len(),
        1,
        "missing raw must remain an ingest error"
    );
    assert!(
        !store::session_shard_dir(
            &raw_missing.path().join("stage"),
            MACHINE,
            "foreign-platform.b83-raw-session"
        )
        .exists(),
        "missing raw must not create a zero-byte archived record"
    );
}

/// D9 rollback proof: the old cursor path accepted NULL session time as the
/// concrete pair `(0, UNIX_EPOCH)`, which is what later rendered as 1970.
#[test]
fn d9_missing_sqlite_time_is_not_a_zero_cursor_or_epoch() {
    let dir = tempfile::tempdir().expect("create sqlite fixture");
    let db = dir.path().join("sessions.sqlite");
    let connection = Connection::open(&db).expect("open synthetic sqlite");
    connection
        .execute_batch(
            "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, updated_at INTEGER); \
             INSERT INTO session_docs (session_id, updated_at) VALUES ('b83-time-session', NULL);",
        )
        .expect("plant NULL session time");
    drop(connection);

    let spec = SqliteSchemaSpec {
        table: "session_docs",
        id_column: Some("session_id"),
        required_columns: vec!["session_id", "updated_at"],
        key_column: None,
        key_prefix: None,
        time_column: Some("updated_at"),
        json_value_column: None,
        json_time_path: None,
        time_is_seconds: true,
        qualification: None,
    };
    let rows = sqlite_probe::enumerate_sqlite_sessions(&db, &spec)
        .expect("enumerate synthetic sqlite session");
    assert_eq!(rows.len(), 1, "the row itself is known");
    assert_eq!(rows[0].time_value, None, "missing time stays None");
    assert_eq!(rows[0].mtime, None, "missing time cannot become UNIX_EPOCH");
    assert!(
        sqlite_probe::sqlite_session_cursor(&db, &spec, "b83-time-session").is_err(),
        "a cursor with no session time must be unknown/error, not time=0"
    );
}

fn cfg_for(repo: &Path, key: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: None,
        no_cache: false,
    }
}

/// D10 rollback proof: make the expected session directory a file. The stage
/// shard walk then returns an actual read error; old `unwrap_or(false)` turned
/// that into `missing_locally=1` and attempted a restore.
#[test]
fn d10_unreadable_stage_shard_check_is_unknown_not_false() {
    let root = tempfile::tempdir().expect("create dest-init fixture");
    let source_stage = root.path().join("source-stage");
    let source_repo = root.path().join("source-repo");
    let source_key = root.path().join("source-key.json");
    let source_cfg = cfg_for(&source_repo, &source_key);
    store::write_sealed_shard(
        StageWriter::Collect,
        &source_stage,
        MACHINE,
        SESSION,
        &["B83 synthetic archive line".to_string()],
    )
    .expect("seal source fixture");
    let master_key = rustic_core::repofile::MasterKey::new();
    store::persist_key_file(&source_cfg, &master_key).expect("persist source key");
    BackupStore::new(source_cfg.clone(), MACHINE.to_string())
        .push(&source_stage, &master_key)
        .expect("push source fixture");

    let target_stage = root.path().join("target-stage");
    let session_path = store::session_shard_dir(&target_stage, MACHINE, SESSION);
    fs::create_dir_all(
        session_path
            .parent()
            .expect("synthetic session parent exists"),
    )
    .expect("create synthetic machine partition");
    write(&session_path, b"not-a-directory");

    let diff = destinit::fill_difference(
        &target_stage,
        MACHINE,
        store::DEFAULT_SHARD_BUCKET_CAP,
        &[SourceDestination {
            name: "synthetic-source".to_string(),
            cfg: source_cfg,
            record: CollectRecord::Present,
        }],
    );
    assert!(
        !diff.diff_complete,
        "unknown local stage coverage is incomplete"
    );
    assert_eq!(diff.sources.len(), 1);
    assert_eq!(diff.sources[0].missing_locally, 0);
    assert_eq!(diff.sources[0].failed_sessions.len(), 1);
    assert_eq!(diff.restored_sessions, 0);
}

fn run_clean_ingest(root: &Path) -> Output {
    let home = root.join("home");
    let xdg_config = root.join("xdg-config");
    let xdg_data = root.join("xdg-data");
    let xdg_state = root.join("xdg-state");
    let xdg_cache = root.join("xdg-cache");
    let inbox_root = root.join("normal-inbox");
    let stage = root.join("normal-stage");
    write(
        &inbox_root.join("deepseek-b83-normal.json"),
        br#"{"schema":"chat-stasher/inbox@1","platform":"deepseek","sessionId":"b83-normal-session","parsed":{"hasJson":true,"keys":["id"]},"raw":{"text":"synthetic normal payload","bytes":24}}"#,
    );
    fs::create_dir_all(&home).expect("create isolated HOME");

    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(["ingest", "--inbox"])
        .arg(&inbox_root)
        .args(["--stage"])
        .arg(&stage)
        .args(["--machine", MACHINE])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_STATE_HOME", &xdg_state)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .output()
        .expect("run isolated normal ingest")
}

#[test]
fn clean_ingest_output_and_exit_code_are_byte_identical() {
    let root = tempfile::tempdir().expect("create normal-path fixture");
    let output = run_clean_ingest(root.path());
    assert_eq!(output.status.code(), Some(0));
    let mut combined = output.stdout;
    combined.extend_from_slice(&output.stderr);
    let actual = sha256_hex(&combined);
    assert_eq!(
        actual, CLEAN_INGEST_OUTPUT_SHA256,
        "clean ingest output bytes changed; actual sha256={actual}"
    );
}
