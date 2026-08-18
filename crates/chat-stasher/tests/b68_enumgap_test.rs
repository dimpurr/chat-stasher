//! B68 — the gap between "counted 414" and "handed over 0".
//!
//! The Cursor probe has always been able to say how many sessions a store
//! *claims* to hold. Turning those claims into `SessionRecord`s is a second,
//! separate step, and that step used to fail silently:
//!
//! ```ignore
//! // scanner.rs, before B68
//! let records = enumerate_cursor_legacy_sessions(&workspace_storage)
//!     .map(|rows| cursor_legacy_records_from_rows(rows, HarnessSource::Cursor, machine))
//!     .unwrap_or_default();           // <- Err becomes an empty Vec ...
//! ...
//! record_count: Some(count),          // <- ... while the count survives
//! ```
//!
//! So did the *classification*: a `composerData` row whose value is NULL and a
//! row that was read and found genuinely empty were both simply "not
//! qualified", indistinguishable in every number the tool printed.
//!
//! These tests pin the three answers the repository refuses to merge:
//!
//!   * **确实没有** — read the body, it really is empty (`filtered`);
//!   * **有但被过滤掉了** — same bucket, by an explicit rule (archived);
//!   * **有但我们读不出来** — the store says a session exists and we could not
//!     get at it (`unreadable`).
//!
//! Everything here runs against `tempfile` fixtures opened `mode=ro` by the
//! code under test. No real harness directory is read, written or renamed.

use chat_stasher::sqlite_probe::{
    cursor_global_schema, probe_cursor_legacy_workspace_storage, probe_sqlite_store_with,
    SqliteSessionProbe,
};
use chat_stasher::{doctor, scanner};
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Env mutation is process-global and cargo runs tests in parallel threads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Cursor's modern global store, planted with one row of every kind that
/// matters. The point of the fixture is that the four rows are
/// *indistinguishable to `count(*)`* and must not be indistinguishable in the
/// report.
///
/// | key                  | value                              | verdict          |
/// |----------------------|------------------------------------|------------------|
/// | `composerData:aaa`   | valid, one conversation header     | handed over      |
/// | `composerData:`      | valid, one conversation header     | **unreadable**   |
/// | `composerData:ccc`   | `NULL`                             | **unreadable**   |
/// | `composerData:ddd`   | valid, zero headers, zero tokens   | filtered (empty) |
///
/// `composerData:` is the "probe counts it, enumeration cannot" case: it
/// satisfies both `key LIKE 'composerData:%'` and the JSON qualification rule,
/// so `count(*)` sees it — but its native session id is the empty string, and
/// `enumerate_sqlite_sessions` skips empty ids with a bare `continue`.
fn plant_cursor_global_db(user_dir: &Path) -> PathBuf {
    let db = user_dir.join("globalStorage").join("state.vscdb");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)")
        .unwrap();
    let with_body = r#"{"composerId":"a","createdAt":1760000000000,"fullConversationHeadersOnly":[{"bubbleId":"b"}]}"#;
    let empty_body =
        r#"{"composerId":"d","createdAt":1760000000000,"fullConversationHeadersOnly":[]}"#;
    for (key, value) in [
        ("composerData:aaa", Some(with_body)),
        ("composerData:", Some(with_body)),
        ("composerData:ccc", None),
        ("composerData:ddd", Some(empty_body)),
    ] {
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
        .unwrap();
    }
    drop(conn);
    db
}

/// A global store with nothing qualified in it, so `probe_cursor_harness`
/// falls through to legacy `workspaceStorage` — the path the reporter's own
/// machine takes.
fn plant_empty_cursor_global_db(user_dir: &Path) -> PathBuf {
    let db = user_dir.join("globalStorage").join("state.vscdb");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)")
        .unwrap();
    drop(conn);
    db
}

/// Cursor's legacy per-workspace stores, one composer of each kind.
///
/// The metadata-only composer is the shape that dominates a real machine: an
/// index entry carrying `composerId`/`createdAt`/`name`/`lastUpdatedAt` and no
/// `conversation` at all, because modern Cursor keeps that body elsewhere.
/// Counting it as "filtered" asserts the session is empty; nothing read here
/// justifies that.
fn plant_cursor_legacy_workspaces(user_dir: &Path) -> PathBuf {
    let workspace_storage = user_dir.join("workspaceStorage");
    let composers = format!(
        r#"{{"allComposers":[{},{},{},{}]}}"#,
        // handed over: an inline conversation with one message
        r#"{"composerId":"w-ok","createdAt":1731000000000,"conversation":[{"type":1}]}"#,
        // filtered: read the body, it is genuinely empty
        r#"{"composerId":"w-empty","createdAt":1731000000000,"conversation":[]}"#,
        // filtered: an explicit rule, not a failure
        r#"{"composerId":"w-arch","createdAt":1731000000000,"isArchived":true,"conversation":[{"type":1}]}"#,
        // unreadable: metadata only, body lives somewhere we cannot reach
        r#"{"composerId":"w-meta","createdAt":1731000000000,"name":"n","lastUpdatedAt":1731000001000}"#,
    );
    let db = workspace_storage.join("ws-1").join("state.vscdb");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)")
        .unwrap();
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES ('composer.composerData', ?1)",
        rusqlite::params![composers],
    )
    .unwrap();
    drop(conn);
    workspace_storage
}

/// A legacy workspace whose composer value is not JSON at all: the store is
/// there, we opened it, and we still know nothing about what is inside.
fn plant_undecodable_legacy_workspace(workspace_storage: &Path) {
    let db = workspace_storage.join("ws-broken").join("state.vscdb");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)")
        .unwrap();
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES ('composer.composerData', ?1)",
        rusqlite::params![b"\x00\x01not json".to_vec()],
    )
    .unwrap();
    drop(conn);
}

/// The shipped macOS Cursor cell, in all three platform slots so the fixture
/// takes one code path on every OS. `env_override` lets the test point the
/// probe at its own `User` directory.
fn write_cursor_registry(home: &Path) -> PathBuf {
    let cursor = r#"{ "template": "~/Library/Application Support/Cursor/User/globalStorage/state.vscdb",
                      "env_override": "CURSOR_USER_DIR", "format": "sqlite",
                      "confidence": "本机实测", "source": "B68 test fixture",
                      "sql_table": "cursorDiskKV", "sql_id_column": "key",
                      "sql_required_columns": ["key", "value"],
                      "sql_key_column": "key", "sql_key_pattern": "composerData:%",
                      "sql_value_column": "value", "sql_time_json_path": "$.createdAt",
                      "sql_qualification": "cursor_composer" }"#;
    let path = home.join("registry.json");
    fs::write(
        &path,
        format!(
            r#"{{ "schema_version": 1, "generated": "B68",
                  "harnesses": [
                    {{ "id": "cursor", "display_name": "Cursor",
                       "paths": {{ "macos": {cursor}, "linux": {cursor}, "windows": {cursor} }} }}
                  ] }}"#
        ),
    )
    .unwrap();
    path
}

/// Point every base-directory variable at `home` so nothing can escape into
/// the real machine, then aim the Cursor probe at `user_dir`.
fn isolate(home: &Path, user_dir: &Path, registry: &Path) {
    std::env::set_var("HOME", home);
    std::env::set_var("USERPROFILE", home);
    std::env::set_var("XDG_DATA_HOME", home.join("xdg-data"));
    std::env::set_var("XDG_CONFIG_HOME", home.join("xdg-config"));
    std::env::set_var("XDG_STATE_HOME", home.join("xdg-state"));
    std::env::set_var("CHAT_STASHER_REGISTRY", registry);
    std::env::set_var("CURSOR_USER_DIR", user_dir);
    for var in ["CODEX_HOME", "GEMINI_CLI_HOME", "OPENCODE_DB"] {
        std::env::remove_var(var);
    }
}

fn cursor_probe(report: &doctor::DoctorReport) -> &scanner::HarnessProbe {
    report
        .probes
        .iter()
        .find(|p| p.id == "cursor")
        .expect("registry probe row missing for cursor")
}

fn cursor_footprint(report: &doctor::DoctorReport) -> &doctor::HarnessFootprint {
    report
        .footprints
        .iter()
        .find(|f| f.name == "cursor")
        .expect("footprint row missing for cursor")
}

// ---------------------------------------------------------------------------
// The unit-level split: filtered vs unreadable
// ---------------------------------------------------------------------------

/// Legacy `workspaceStorage`: four composers, four different reasons, and the
/// probe has to keep the failure separate from the two genuine filters.
///
/// This is the exact shape behind the reporter's `过滤前 414 / 过滤后 3`: the
/// composers that vanished are metadata-only index entries, so 411 of them
/// belong in "读不出来", not in "确实没有".
#[test]
fn legacy_probe_separates_a_missing_body_from_an_empty_one() {
    let base = tempfile::tempdir().unwrap();
    let workspace_storage = plant_cursor_legacy_workspaces(base.path());
    plant_undecodable_legacy_workspace(&workspace_storage);

    let probe = probe_cursor_legacy_workspace_storage(&workspace_storage)
        .probe
        .expect("fixture must be probeable");
    let SqliteSessionProbe::Known {
        candidate_count,
        count,
        ..
    } = probe.sessions
    else {
        panic!(
            "fixture must produce a Known probe, got {:?}",
            probe.sessions
        );
    };

    assert_eq!(candidate_count, 4, "四个 composer 都应被计为候选");
    assert_eq!(count, 1, "只有带非空 conversation 的那一个是合格会话");
    assert_eq!(
        probe.unreadable_count, 1,
        "只有 metadata-only 那一个属于「读不出来」"
    );
    // The two genuine filters — empty conversation, archived — must not be
    // reported as failures. A false positive is its own kind of lie.
    assert_eq!(
        candidate_count - count - probe.unreadable_count,
        2,
        "空 conversation 与 archived 是「确实没有」，不许算成失败"
    );
    assert_eq!(
        probe.unreadable_stores, 1,
        "打不开/读不懂的那个库要单独计数，不能当成 0 个会话"
    );
}

/// The modern global store: a `NULL` value is dropped by the qualification
/// rule's `json_valid(value)=1`, which made it look exactly like a row that
/// had been read and found empty.
#[test]
fn global_probe_counts_an_undecodable_row_as_unreadable_not_as_empty() {
    let base = tempfile::tempdir().unwrap();
    let db = plant_cursor_global_db(base.path());
    // The same schema the registry cell declares, exported for the collect
    // side; the doctor-level tests below exercise the registry-driven path.
    let probe = probe_sqlite_store_with(&db, &cursor_global_schema());
    let SqliteSessionProbe::Known {
        candidate_count,
        count,
        ..
    } = probe.sessions
    else {
        panic!(
            "fixture must produce a Known probe, got {:?}",
            probe.sessions
        );
    };

    assert_eq!(candidate_count, 4, "四行 composerData 都是候选");
    assert_eq!(count, 2, "两行带非空 header 的行通过过滤");
    assert_eq!(
        probe.unreadable_count, 1,
        "value 为 NULL 的那行是「读不出来」，不是「读了发现是空的」"
    );
    assert_eq!(
        candidate_count - count - probe.unreadable_count,
        1,
        "header 为空的那行是「确实没有」，不许算成失败"
    );
}

// ---------------------------------------------------------------------------
// The end-to-end number: known vs handed over
// ---------------------------------------------------------------------------

/// The headline case. The global store hands the scanner two qualified rows and
/// only one of them can become a `SessionRecord`; a third row could not be
/// decoded at all. Before B68 the probe reported `record_count: Some(2)` beside
/// one emitted record and said nothing — "数出来 2 条，交出来 1 条".
#[test]
fn doctor_says_how_many_known_sessions_it_could_not_hand_over() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let base = tempfile::tempdir().unwrap();
    let home = base.path().join("home");
    let user_dir = home.join("Cursor").join("User");
    fs::create_dir_all(&home).unwrap();
    plant_cursor_global_db(&user_dir);
    let registry = write_cursor_registry(base.path());
    isolate(&home, &user_dir, &registry);

    let report = doctor::run();
    let probe = cursor_probe(&report);
    let footprint = cursor_footprint(&report);

    assert_eq!(probe.candidate_count, Some(4), "已知候选 4 条");
    assert_eq!(probe.record_count, Some(2), "过滤后 2 条");
    // 1 undecodable candidate + 1 qualified row enumeration could not turn
    // into a record = 2 sessions this machine knows about and cannot hand over.
    assert_eq!(
        probe.unreadable_count,
        Some(2),
        "「读不出来」必须被计数，而不是悄悄变成空集合"
    );
    assert_eq!(
        footprint.unreadable_count,
        Some(2),
        "footprint 行与 registry 行必须报同一个数"
    );
    assert!(
        probe.note.contains("读不出来"),
        "这个数必须出现在用户看得见的那行里，实际 note = {}",
        probe.note
    );
}

/// The same invariant on the legacy fallback path — the path the reporter's
/// machine actually takes, where `enumerate_cursor_legacy_sessions`'s `Err`
/// used to be swallowed by `.unwrap_or_default()`.
#[test]
fn legacy_fallback_reports_the_metadata_only_composers_it_cannot_read() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let base = tempfile::tempdir().unwrap();
    let home = base.path().join("home");
    let user_dir = home.join("Cursor").join("User");
    fs::create_dir_all(&home).unwrap();
    plant_empty_cursor_global_db(&user_dir);
    plant_cursor_legacy_workspaces(&user_dir);
    let registry = write_cursor_registry(base.path());
    isolate(&home, &user_dir, &registry);

    let report = doctor::run();
    let probe = cursor_probe(&report);

    assert_eq!(probe.candidate_count, Some(4), "已知候选 4 条");
    assert_eq!(probe.record_count, Some(1), "过滤后 1 条");
    assert_eq!(
        probe.unreadable_count,
        Some(1),
        "metadata-only composer 要作为「读不出来」被计数"
    );
    assert!(
        probe.note.contains("读不出来"),
        "这个数必须出现在用户看得见的那行里，实际 note = {}",
        probe.note
    );
}

/// The false-positive guard, and the "一切正常时输出不变" guard in one: a store
/// whose every dropped row was genuinely read and found empty must report
/// **zero** unreadable sessions and must not add a word to its line.
#[test]
fn a_store_that_only_filters_never_claims_anything_was_unreadable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let base = tempfile::tempdir().unwrap();
    let home = base.path().join("home");
    let user_dir = home.join("Cursor").join("User");
    fs::create_dir_all(&home).unwrap();

    // Two rows: one qualified and enumerable, one read and found empty.
    let db = user_dir.join("globalStorage").join("state.vscdb");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)")
        .unwrap();
    conn.execute(
        "INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:aaa', ?1)",
        rusqlite::params![
            r#"{"composerId":"a","createdAt":1760000000000,"fullConversationHeadersOnly":[{"bubbleId":"b"}]}"#
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:ddd', ?1)",
        rusqlite::params![
            r#"{"composerId":"d","createdAt":1760000000000,"fullConversationHeadersOnly":[]}"#
        ],
    )
    .unwrap();
    drop(conn);

    let registry = write_cursor_registry(base.path());
    isolate(&home, &user_dir, &registry);

    let report = doctor::run();
    let probe = cursor_probe(&report);

    assert_eq!(probe.candidate_count, Some(2));
    assert_eq!(probe.record_count, Some(1));
    assert_eq!(
        probe.unreadable_count,
        Some(0),
        "被正常过滤掉的行不许被报成失败 —— 假阳性也是一种谎"
    );
    assert!(
        !probe.note.contains("读不出来"),
        "一切正常时这行输出必须和 B68 之前一模一样，实际 note = {}",
        probe.note
    );
    assert_eq!(
        cursor_footprint(&report).unreadable_count,
        Some(0),
        "footprint 行同样不许无中生有"
    );
}
