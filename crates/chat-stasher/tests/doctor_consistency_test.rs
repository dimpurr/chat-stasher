//! The anti-regression test this spike exists for: `doctor` must never print
//! two contradictory numbers for the same harness.
//!
//! This is deliberately **not** "the opencode test" — it loops over *every*
//! harness that appears in both doctor tables (the footprint table and the
//! registry probe table) and asserts the two agree on session count, and on
//! bytes for single-file stores. If a future harness is added to both tables
//! and someone wires its count into only one path (the bug fixed here, and
//! before that for Gemini), this test goes red.
//!
//! The real opening-can-fail proof lives in the task report; mechanically this
//! test is built so that *any* divergence between the two tables fails.

use chat_stasher::doctor::{self, HarnessFootprint};
use chat_stasher::scanner::{self, HarnessProbe};
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

/// How many sessions exist in each harness store the test plants, so the
/// assertion is against known counts, not just "the two numbers are equal".
const CLAUDE_SESSIONS: u64 = 2;
const CODEX_SESSIONS: u64 = 1;
const GEMINI_SESSIONS: u64 = 2;
const OPENCODE_SESSIONS: u64 = 3;
const CURSOR_SESSIONS: u64 = 2;
const GROK_SESSIONS: u64 = 1;

fn write(path: &Path, content: &str) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, content).unwrap();
}

/// Plant one real SQLite store (`session(id, time_created, time_updated)`)
/// with `n` rows — the recognised opencode schema. The test sets
/// `XDG_DATA_HOME` to `home`, so this is the registry's middle fallback.
fn plant_opencode_db(home: &Path) {
    let db = home.join("opencode/opencode.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE session(id TEXT PRIMARY KEY, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL)",
    )
    .unwrap();
    for i in 0..OPENCODE_SESSIONS {
        conn.execute(
            "INSERT INTO session (id, time_created, time_updated) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                format!("test-{i}"),
                1760000000000i64 + i as i64,
                1760000000000i64
            ],
        )
        .unwrap();
    }
    drop(conn);
}

/// Plant the other three harnesses' session files inside `home`.
fn plant_dir_sessions(home: &Path) {
    for i in 0..CLAUDE_SESSIONS {
        write(
            &home.join(format!(".claude/projects/repo-abc/019bf00d-{i:04}.jsonl")),
            "{}\n",
        );
    }
    for i in 0..CODEX_SESSIONS {
        write(
            &home.join(format!(".codex/sessions/2026-08-01/019bf00d-{i:04}.jsonl")),
            "{}\n",
        );
    }
    write(&home.join(".gemini/tmp/session-a.json"), "{}\n");
    write(&home.join(".gemini/tmp/session-b.jsonl"), "{}\n");
    // A non-session JSON that the registry session_pattern must reject.
    write(&home.join(".gemini/tmp/settings.json"), "{}\n");
}

/// Plant a Cursor-shaped store: `cursorDiskKV`, sessions selected by
/// `composerData:%`, timestamps via `createdAt` inside the JSON value.
fn plant_cursor_db(home: &Path) {
    let db = home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
    )
    .unwrap();
    for i in 0..CURSOR_SESSIONS {
        let key = format!("composerData:00000000-0000-4000-8000-{i:012}");
        let value = format!(
            r#"{{"composerId":"c{i}","createdAt":{},"fullConversationHeadersOnly":[{{}}]}}"#,
            1760000000000i64 + i as i64
        );
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
        .unwrap();
    }
    drop(conn);
}

/// Plant a Grok-shaped store: `session_docs`, all rows, `updated_at` seconds.
/// The store is WAL-mode with its sidecars removed afterwards — the real-world
/// "app closed cleanly, no -wal/-shm" state — so the probe must fall back to
/// the immutable read-only path (a plain `mode=ro` open cannot read a WAL
/// store whose shared-memory file does not exist).
fn plant_grok_db(home: &Path) {
    let db = home.join(".grok/sessions/session_search.sqlite");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL").unwrap();
    conn.execute_batch(
        "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL, updated_at INTEGER NOT NULL, title TEXT NOT NULL, content TEXT NOT NULL, content_hash TEXT NOT NULL, last_indexed_offset INTEGER NOT NULL DEFAULT 0)",
    )
    .unwrap();
    for i in 0..GROK_SESSIONS {
        conn.execute(
            "INSERT INTO session_docs (session_id, cwd, updated_at, title, content, content_hash) VALUES (?1,?2,?3,?4,?5,?6)",
            rusqlite::params![
                format!("019f95ce-{i:08}-0000-0000-0000-000000000000"),
                "/tmp/x",
                1784924765i64 + i as i64,
                "t",
                "c",
                "h"
            ],
        )
        .unwrap();
    }
    drop(conn);
    // The last-close checkpoint normally removes the sidecars; be explicit
    // about the no-sidecar state so the immutable fallback is what gets tested.
    let mut wal = db.as_os_str().to_os_string();
    wal.push("-wal");
    let mut shm = db.as_os_str().to_os_string();
    shm.push("-shm");
    #[allow(
        clippy::let_underscore_must_use,
        reason = "The test explicitly creates the no-sidecar state; cleanup is best-effort after the state is established."
    )]
    let _ = fs::remove_file(Path::new(&wal));
    #[allow(
        clippy::let_underscore_must_use,
        reason = "The test explicitly creates the no-sidecar state; cleanup is best-effort after the state is established."
    )]
    let _ = fs::remove_file(Path::new(&shm));
}

/// Tell the tool where the test planted the two single-file stores.
///
/// The fixture above plants Cursor and Grok at paths *it* chose, so the shipped
/// registry's per-platform template is not what should decide whether they are
/// found: on linux the Cursor cell points at `$XDG_CONFIG_HOME/Cursor/...` and
/// the Grok cell is `unascertained` (correctly refused as a guess). Writing the paths
/// into `[harness_roots]` is the test stating what it did — the same thing a
/// user with a non-default install does — so the fixture resolves identically
/// on every platform. The known-count assertions below are unchanged: the
/// tool still has to open the stores and count the rows itself.
fn declare_planted_roots(home: &Path) {
    let cursor = home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb");
    let grok = home.join(".grok/sessions/session_search.sqlite");
    let config = home.join(".config/chat-stasher/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        format!(
            "[harness_roots]\ncursor = \"{}\"\ngrok = \"{}\"\n",
            cursor.display(),
            grok.display()
        ),
    )
    .unwrap();
}

/// Link a footprint table row to its registry probe-table row.
///
/// Footprint names are the map keys; probe ids are registry `id` values — they
/// differ for gemini (`gemini` vs `gemini-cli`), so the join is explicit
/// rather than guessed by string equality.
const FOOTPRINT_TO_PROBE_ID: [(&str, &str); 6] = [
    ("claude-code", "claude-code"),
    ("codex", "codex"),
    ("gemini", "gemini-cli"),
    ("opencode", "opencode"),
    ("cursor", "cursor"),
    ("grok", "grok"),
];

/// The consistency invariant shared by every harness in both tables: the two
/// tables must never contradict on session count, on recognised file identity,
/// or on bytes for single-file stores.
fn assert_report_self_consistent(report: &doctor::DoctorReport) {
    for (fp_name, probe_id) in FOOTPRINT_TO_PROBE_ID {
        let fp: &HarnessFootprint = report
            .footprints
            .iter()
            .find(|f| f.name == fp_name)
            .unwrap_or_else(|| panic!("footprint row missing for {fp_name}"));
        let probe: &HarnessProbe = report
            .probes
            .iter()
            .find(|p| p.id == probe_id)
            .unwrap_or_else(|| panic!("registry probe row missing for {probe_id}"));

        // Session count: two identical tables must not print different numbers
        // for the same harness. `None` (not enumerable) is consistent only
        // with `None`; a real count must equal a real count.
        match (fp.session_count, probe.record_count) {
            (Some(a), Some(b)) => assert_eq!(
                a, b,
                "harness {fp_name} contradicts itself: footprint table sessions {a} vs registry table sessions {b}"
            ),
            (None, None) => {}
            (a, b) => panic!(
                "harness {fp_name} inconsistent session counting: footprint={a:?} registry={b:?} (one can enumerate while the other cannot, or vice versa)"
            ),
        }

        // Directory harnesses must recognise the same file set, not merely
        // happen to report the same aggregate count. This catches a `.jsonl`
        // omission even when another implementation path still reports one
        // plausible session.
        if matches!(probe.state, scanner::ProbeState::Scanned) {
            let doctor_files: BTreeSet<_> = fp.recognized_files.iter().collect();
            let scanner_files: BTreeSet<_> = probe.recognized_files.iter().collect();
            assert!(
                doctor_files == scanner_files,
                "harness {fp_name} file recognition inconsistency: doctor={}/scanner={}",
                doctor_files.len(),
                scanner_files.len()
            );
        }

        // Bytes: for single-file stores both tables measure the same set
        // (`.db` + `-wal` + `-shm`); they must agree byte for byte.
        if matches!(probe.state, scanner::ProbeState::FileTarget) {
            assert_eq!(
                fp.total_bytes, probe.bytes,
                "harness {fp_name} byte count inconsistency: footprint={:?} B vs registry={:?} B",
                fp.total_bytes, probe.bytes
            );
        }
    }
}

/// Regression: `doctor` may not show two contradictory numbers for the same
/// harness (previously: footprint said opencode 243 / registry table said 0,
/// because the SQLite enumeration only reached one of the two paths).
#[test]
fn doctor_tables_never_contradict_any_harness() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    std::env::set_var("XDG_DATA_HOME", home.path());
    std::env::remove_var("XDG_CONFIG_HOME");
    std::env::remove_var("XDG_STATE_HOME");

    plant_opencode_db(home.path());
    plant_dir_sessions(home.path());
    plant_cursor_db(home.path());
    plant_grok_db(home.path());
    declare_planted_roots(home.path());

    let report = doctor::run();
    assert!(!report.scan_failed, "registry scan must have run");

    // Run the cross-table invariant before the per-harness ground truths. A
    // broken doctor-only filter must fail here, rather than being hidden by
    // two independently wrong counts.
    assert_report_self_consistent(&report);

    // Negative self-check against known ground truth: a test that only
    // compares two wrong numbers to each other could go green by accident.
    let gemini = report
        .footprints
        .iter()
        .find(|f| f.name == "gemini")
        .unwrap();
    println!(
        "gemini_fake_dir_count={} (session-a.json + session-b.jsonl; settings.json excluded)",
        gemini.session_count.unwrap_or(0)
    );
    assert_eq!(
        gemini.session_count,
        Some(GEMINI_SESSIONS),
        "doctor dummy directory should recognize both .json/.jsonl sessions and exclude settings.json"
    );

    let opencode = report
        .footprints
        .iter()
        .find(|f| f.name == "opencode")
        .unwrap();
    assert_eq!(
        opencode.session_count,
        Some(OPENCODE_SESSIONS),
        "footprint table self-check against known ground truth: should recognize our {OPENCODE_SESSIONS} planted SQLite sessions"
    );
    let cursor = report
        .footprints
        .iter()
        .find(|f| f.name == "cursor")
        .unwrap();
    assert_eq!(
        cursor.session_count,
        Some(CURSOR_SESSIONS),
        "footprint table self-check against known ground truth: should recognize our {CURSOR_SESSIONS} planted cursorDiskKV sessions"
    );
    let grok = report.footprints.iter().find(|f| f.name == "grok").unwrap();
    assert_eq!(
        grok.session_count,
        Some(GROK_SESSIONS),
        "footprint table self-check against known ground truth: should recognize our {GROK_SESSIONS} planted session_docs sessions"
    );
}

/// An existing path with the wrong shape is not a measured absence. Doctor's
/// footprint summary must keep the scanner's indeterminate reason visible and
/// its counts unknown in both output formats.
#[test]
fn doctor_keeps_an_unavailable_platform_source_unknown_in_both_outputs() {
    let sandbox = tempfile::tempdir().unwrap();
    let home = sandbox.path().join("home");
    let source = sandbox.path().join("synthetic-platform-source");
    let registry = sandbox.path().join("registry.json");
    fs::create_dir_all(&home).unwrap();
    // The synthetic Codex registry cell declares a directory, but the source
    // is a file. This deterministically exercises an unavailable platform
    // source on all platforms without relying on chmod or host permissions.
    fs::write(&source, b"synthetic unavailable source\n").unwrap();
    fs::write(
        &registry,
        format!(
            r#"{{"schema_version":1,"generated":"synthetic doctor fixture","harnesses":[{{"id":"codex","display_name":"Synthetic Codex","paths":{{"macos":{{"template":"{}/","format":"jsonl","confidence":"source-confirmed","source":"synthetic fixture"}},"linux":{{"template":"{}/","format":"jsonl","confidence":"source-confirmed","source":"synthetic fixture"}},"windows":{{"template":"{}/","format":"jsonl","confidence":"source-confirmed","source":"synthetic fixture"}}}}}}]}}"#,
            source.display(),
            source.display(),
            source.display()
        ),
    )
    .unwrap();

    let run_doctor = |json: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        command
            .arg("doctor")
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", sandbox.path().join("config"))
            .env("XDG_DATA_HOME", sandbox.path().join("data"))
            .env("XDG_STATE_HOME", sandbox.path().join("state"))
            .env("XDG_CACHE_HOME", sandbox.path().join("xdg-cache"))
            .env("CHAT_STASHER_REGISTRY", &registry)
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(sandbox.path()),
            );
        if json {
            command.arg("--json");
        }
        command.output().expect("run isolated doctor")
    };

    let human = run_doctor(false);
    assert!(human.status.success());
    let human = String::from_utf8_lossy(&human.stderr);
    assert!(
        human.contains("sessions unknown") && human.contains("path exists but is not a directory"),
        "human doctor output must preserve the unknown source and its reason; output:\n{human}"
    );
    assert!(
        !human.contains("codex      not installed"),
        "an indeterminate source must not be rendered as measured absence; output:\n{human}"
    );

    let json = run_doctor(true);
    assert!(json.status.success());
    let stdout = String::from_utf8_lossy(&json.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json must emit JSON ({e}): {stdout}"));
    let footprint = report["footprints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "codex")
        .unwrap();
    assert_eq!(footprint["session_count"]["kind"], "unknown");
    assert_eq!(footprint["total_bytes"]["kind"], "unknown");
    assert!(
        footprint["note"]
            .as_str()
            .unwrap()
            .contains("path exists but is not a directory"),
        "JSON footprint must preserve the unreadable source reason: {footprint}"
    );
    assert_ne!(footprint["session_count"]["count"], 0);
    assert_ne!(footprint["total_bytes"]["count"], 0);
}
